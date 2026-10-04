//! Turns a canonical publication into a deployable set of files.
//!
//! This module downloads published mods by their exact `VersionIdent` and
//! extracts them with the game's package installers into a staging
//! directory. The install queue uses the same machinery, so the staged
//! tree is byte-identical to what a subscriber's profile contains. No mod
//! is ever resolved to a newer version; the publication's exact versions
//! are authoritative.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use eyre::{Context, Result};
use futures_util::future::BoxFuture;
use reqwest_middleware::ClientWithMiddleware;
use tracing::info;
use walkdir::WalkDir;
use zip::ZipArchive;

use super::{
    paths::DeployPathBuf,
    plan::{DesiredDeployment, FileSource, StagedFile},
    progress::{ProgressReporter, SyncPhase},
    spec::DeploymentSpec,
};
use crate::{
    game::mod_loader::ModLoader,
    profile::{
        export::{ContentHash, R2Mod},
        install::{InstallError, download},
        sync::FetchedPublication,
    },
    thunderstore::{Backend, VersionIdent},
    util,
};

/// How package payloads are obtained. Local mode and the worker share this
/// contract; each supplies its own cache directory and HTTP client.
pub trait PayloadSource: Send + Sync {
    /// Ensures the extracted package tree for `ident` exists, downloading
    /// and extracting it when missing. Returns the tree's root.
    fn stage<'a>(
        &'a self,
        ident: &'a VersionIdent,
        backend: Backend,
    ) -> BoxFuture<'a, Result<PathBuf>>;
}

/// A payload source backed by a plain directory plus an HTTP client.
///
/// Layout: `<root>/<full_name>/<version>/`, identical to Gale's install
/// cache so Local mode can share `prefs.cache_dir()` directly.
pub struct CachePayloadSource {
    pub root: PathBuf,
    pub client: ClientWithMiddleware,
    pub mod_loader: &'static ModLoader<'static>,
}

impl PayloadSource for CachePayloadSource {
    fn stage<'a>(
        &'a self,
        ident: &'a VersionIdent,
        backend: Backend,
    ) -> BoxFuture<'a, Result<PathBuf>> {
        Box::pin(async move {
            let parent = self.root.join(ident.full_name());
            let dest = parent.join(ident.version());

            if is_staged(&dest) {
                return Ok(dest);
            }

            let url = backend.download_url(ident);
            let bytes = download::fetch(&self.client, &url, 0, &|_| {}, &|| Ok(()))
                .await
                .map_err(|err| match err {
                    InstallError::Error(err) => err,
                    InstallError::Cancelled => eyre::eyre!("the download was cancelled"),
                })
                .with_context(|| format!("failed to download {ident}"))?;

            std::fs::create_dir_all(&parent).context("failed to create staging directory")?;

            // Extract to a sibling temp dir, then rename into place so an
            // interrupted extraction can't leave a half-populated tree
            // behind.
            let tmp = tempfile::tempdir_in(&parent)?;
            self.extract(bytes, ident, tmp.path())?;
            commit_staged(tmp, &dest)?;
            Ok(dest)
        })
    }
}

fn commit_staged(tmp: tempfile::TempDir, dest: &Path) -> Result<()> {
    if is_staged(dest) {
        return Ok(());
    }
    if dest.exists() {
        // Remove only an empty cache entry. A concurrent writer must not
        // lose files, and an incomplete tree must never be copied into view.
        std::fs::remove_dir(dest)?;
    }
    std::fs::rename(tmp.path(), dest).context("failed to publish staged package")
}

impl CachePayloadSource {
    fn extract(&self, bytes: Vec<u8>, ident: &VersionIdent, dest: &Path) -> Result<()> {
        let archive = ZipArchive::new(Cursor::new(bytes))
            .with_context(|| format!("{ident} download is not a valid zip"))?;
        let mut installer = self.mod_loader.installer_for(ident.full_name());
        installer
            .extract(archive, ident.full_name(), dest.to_path_buf())
            .with_context(|| format!("failed to extract {ident} for server deployment"))
    }
}

fn is_staged(dir: &Path) -> bool {
    dir.is_dir()
        && dir
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some())
}

/// Stages the same published payload and reports work for either executor.
pub async fn stage_fetched(
    publication: &FetchedPublication,
    cache_dir: PathBuf,
    client: ClientWithMiddleware,
    mod_loader: &'static ModLoader<'static>,
    spec: &DeploymentSpec,
    include_mods: bool,
    progress: &mut ProgressReporter,
) -> Result<DesiredDeployment> {
    if include_mods {
        progress.phase(SyncPhase::StagingPayload);
    }
    let source = CachePayloadSource {
        root: cache_dir,
        client,
        mod_loader,
    };
    let mut staging_total = None;
    stage_publication(
        publication,
        &source,
        spec,
        include_mods,
        |completed, total, name| {
            if staging_total.is_none() {
                progress.work(total, None);
                staging_total = Some(total);
            }
            if !name.is_empty() {
                progress.item(name);
            }
            progress.advance(completed, None);
        },
    )
    .await
}

/// Reads the staged package trees and builds the desired deployment file
/// maps. Only enabled mods contribute files; the planner removes previously
/// deployed files that are no longer in this map.
///
/// `progress` receives `(completed, total, mod_name)` for reporting.
pub async fn stage_publication(
    publication: &FetchedPublication,
    source: &dyn PayloadSource,
    spec: &DeploymentSpec,
    include_mods: bool,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<DesiredDeployment> {
    let mut desired = DesiredDeployment::default();
    if !include_mods {
        return Ok(desired);
    }

    let enabled: Vec<&R2Mod> = publication
        .manifest
        .mods
        .iter()
        .filter(|m| m.enabled)
        .collect();
    let total = enabled.len();

    for (index, r2mod) in enabled.iter().enumerate() {
        let ident = r2mod.version_ident();
        progress(index, total, ident.full_name());

        let tree = source.stage(&ident, r2mod.source).await?;
        collect_tree(&tree, spec, &mut desired)
            .with_context(|| format!("failed to stage files of {ident}"))?;
    }
    progress(total, total, "");

    info!(payload = desired.len(), "staged published mod set");

    Ok(desired)
}

/// Collects the payload files in one staged package tree.
fn collect_tree(root: &Path, spec: &DeploymentSpec, desired: &mut DesiredDeployment) -> Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.context("failed to enumerate staged package")?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .context("staged file escaped its root")?;
        let relative = deploy_path(relative)?;

        // `.old` files are disabled variants and never deployed.
        if relative.file_name().ends_with(".old") {
            continue;
        }
        // `deploys` also rejects config-dir files inside packages: the
        // running server generates or migrates its configs, and only an
        // explicit config push writes them.
        if spec.is_gale_internal(&relative) || !spec.deploys(&relative, false) {
            continue;
        }

        let file = staged_file(entry.path(), &relative)?;
        desired.insert(relative, file);
    }

    Ok(())
}

fn staged_file(path: &Path, relative: &DeployPathBuf) -> Result<StagedFile> {
    let metadata = path
        .metadata()
        .with_context(|| format!("failed to read staged file {}", path.display()))?;
    let hash = ContentHash::from_hash(
        util::fs::checksum(path).with_context(|| format!("failed to hash {relative}"))?,
    );

    Ok(StagedFile {
        source: FileSource::Path(path.to_path_buf()),
        hash,
        size: metadata.len(),
    })
}

/// Converts a local relative path into its canonical deploy-path form.
fn deploy_path(path: &Path) -> Result<DeployPathBuf> {
    let value = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| eyre::eyre!("staged file has a non-Unicode path: {}", path.display()))?
        .join("/");

    DeployPathBuf::new(value)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use futures_util::future::BoxFuture;

    use super::{PayloadSource, stage_publication};
    use crate::{
        game::mod_loader::{ModLoader, ModLoaderKind},
        profile::{
            export::{ModRevision, ProfileManifest, R2Mod},
            server::spec::DeploymentSpec,
            sync::FetchedPublication,
        },
        thunderstore::{Backend, PackageIdent, VersionIdent},
    };

    fn publication(mods: Vec<R2Mod>) -> FetchedPublication {
        FetchedPublication {
            revision: Utc::now(),
            mods_revision: ModRevision::from_hash(blake3::hash(b"rev")),
            manifest: ProfileManifest {
                name: String::new(),
                mods,
                game: None,
                ignored_version_updates: Vec::new(),
                ignored_package_updates: Vec::new(),
                sync: None,
            },
            config: BTreeMap::new(),
        }
    }

    /// A payload source that must never be touched. Any call to it fails
    /// the test, which proves configs-only operations never stage mod
    /// packages.
    struct FailSource;

    impl PayloadSource for FailSource {
        fn stage<'a>(
            &'a self,
            ident: &'a VersionIdent,
            _backend: Backend,
        ) -> BoxFuture<'a, eyre::Result<std::path::PathBuf>> {
            panic!("mod payload source was requested for {}", ident.full_name());
        }
    }

    fn bepinex() -> ModLoader<'static> {
        ModLoader {
            package_name: None,
            file_target: None,
            kind: ModLoaderKind::BepInEx {
                extra_subdirs: Vec::new(),
            },
        }
    }

    fn spec() -> DeploymentSpec {
        DeploymentSpec::for_loader(&bepinex()).unwrap()
    }

    #[test]
    fn extracted_packages_use_the_install_cache_layout() {
        // Local mode stages into Gale's install cache, and a later install
        // of the same version reuses whatever is there. BepInEx installs
        // keep each mod's plugins in a folder named after the full package
        // name, so staging must lay the package out the same way.
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("plugins/Mod.dll", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"dll").unwrap();
        let archive = zip.finish().unwrap().into_inner();

        let source = super::CachePayloadSource {
            root: std::path::PathBuf::new(),
            client: reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
            mod_loader: Box::leak(Box::new(bepinex())),
        };
        let ident = VersionIdent::from(("Author", "Mod", "1.0.0"));
        let dest = tempfile::tempdir().unwrap();

        source.extract(archive, &ident, dest.path()).unwrap();

        assert_eq!(
            std::fs::read(dest.path().join("BepInEx/plugins/Author-Mod/Mod.dll")).unwrap(),
            b"dll"
        );
    }

    #[test]
    fn staging_replaces_empty_cache_entries_and_preserves_completed_entries() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("package");
        std::fs::create_dir(&dest).unwrap();
        for contents in ["complete", "concurrent download"] {
            let tmp = tempfile::tempdir_in(root.path()).unwrap();
            std::fs::write(tmp.path().join("plugin.dll"), contents).unwrap();
            super::commit_staged(tmp, &dest).unwrap();
            assert_eq!(
                std::fs::read_to_string(dest.join("plugin.dll")).unwrap(),
                "complete"
            );
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn configs_only_staging_never_touches_the_mod_source() {
        // Local/Worker parity regression: a configs-only operation must
        // not require downloading or extracting any mod package, even
        // when the publication contains enabled mods.
        let mods = vec![R2Mod {
            ident: PackageIdent::from(("Author", "SomeMod")),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }];
        let publication = publication(mods);

        let desired = stage_publication(&publication, &FailSource, &spec(), false, |_, _, _| {})
            .await
            .unwrap();

        assert!(desired.is_empty());
    }

    /// Serves a prepared package tree from disk.
    struct StaticSource(std::path::PathBuf);

    impl PayloadSource for StaticSource {
        fn stage<'a>(
            &'a self,
            _ident: &'a VersionIdent,
            _backend: Backend,
        ) -> BoxFuture<'a, eyre::Result<std::path::PathBuf>> {
            let root = self.0.clone();
            Box::pin(async move { Ok(root) })
        }
    }

    #[tokio::test]
    async fn packaged_config_files_are_never_staged() {
        // A mod package can bundle files under config dirs. Those are
        // server-owned territory. The running server generates or
        // migrates them, so they must not enter the deployment set even
        // when the payload is staged for a mods deployment.
        let package = tempfile::tempdir().unwrap();
        let config_dir = package.path().join("BepInEx/config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("default.cfg"), b"package-default").unwrap();
        let plugin_dir = package.path().join("BepInEx/plugins/Mod");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("Mod.dll"), b"dll").unwrap();

        let mods = vec![R2Mod {
            ident: PackageIdent::from(("Author", "Mod")),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }];
        let publication = publication(mods);
        let source = StaticSource(package.path().to_path_buf());

        let desired = stage_publication(&publication, &source, &spec(), true, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(desired.len(), 1);
        assert!(
            desired
                .keys()
                .all(|path| path.as_str() == "BepInEx/plugins/Mod/Mod.dll"),
            "the bundled config must not enter the deployment set"
        );
    }
}
