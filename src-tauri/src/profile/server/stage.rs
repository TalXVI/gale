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

use eyre::{Context, Result, ensure};
use futures_util::future::BoxFuture;
use tracing::info;
use walkdir::WalkDir;
use zip::ZipArchive;

use super::{
    paths::DeployPathBuf,
    plan::{DesiredDeployment, FileSource, Publication, StagedFile},
    spec::DeploymentSpec,
};
use crate::{
    game::mod_loader::ModLoader,
    profile::export::R2Mod,
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

/// A payload source backed by a plain directory plus a reqwest client.
///
/// Layout: `<root>/<full_name>/<version>/`, identical to Gale's install
/// cache so Local mode can share `prefs.cache_dir()` directly.
pub struct CachePayloadSource {
    pub root: PathBuf,
    pub client: reqwest::Client,
    pub mod_loader: &'static ModLoader<'static>,
}

impl PayloadSource for CachePayloadSource {
    fn stage<'a>(
        &'a self,
        ident: &'a VersionIdent,
        backend: Backend,
    ) -> BoxFuture<'a, Result<PathBuf>> {
        Box::pin(async move {
            let dest = self.root.join(ident.full_name()).join(ident.version());

            if is_staged(&dest) {
                return Ok(dest);
            }

            let url = backend.download_url(ident);
            let bytes = self
                .client
                .get(&url)
                .send()
                .await
                .and_then(|res| res.error_for_status())
                .with_context(|| format!("failed to download {ident}"))?
                .bytes()
                .await
                .with_context(|| format!("failed to read {ident} download"))?;

            let parent = dest
                .parent()
                .ok_or_else(|| eyre::eyre!("invalid staging path"))?;
            std::fs::create_dir_all(parent).context("failed to create staging directory")?;

            // Extract to a sibling temp dir, then rename into place so an
            // interrupted extraction can't leave a half-populated tree
            // behind.
            let tmp = tempfile::tempdir_in(parent)?;
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
    fn extract(&self, bytes: bytes::Bytes, ident: &VersionIdent, dest: &Path) -> Result<()> {
        let archive = ZipArchive::new(Cursor::new(bytes.to_vec()))
            .with_context(|| format!("{ident} download is not a valid zip"))?;
        let mut installer = self.mod_loader.installer_for(ident.full_name());
        installer
            .extract(archive, ident.name(), dest.to_path_buf())
            .with_context(|| format!("failed to extract {ident} for server deployment"))
    }
}

fn is_staged(dir: &Path) -> bool {
    dir.is_dir()
        && dir
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some())
}

/// Reads the staged package trees and builds the desired deployment file
/// maps. Only enabled mods contribute files. A disabled mod's files leave
/// the server entirely, matching how a subscriber's profile handles it.
///
/// `progress` receives `(completed, total, mod_name)` for reporting.
pub async fn stage_publication(
    publication: &Publication<'_>,
    source: &dyn PayloadSource,
    spec: &DeploymentSpec,
    include_mods: bool,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<DesiredDeployment> {
    let mut desired = DesiredDeployment::default();
    if !include_mods {
        return Ok(desired);
    }

    let enabled: Vec<&R2Mod> = publication.mods.iter().filter(|m| m.enabled).collect();
    let total = enabled.len();

    for (index, r2mod) in enabled.iter().enumerate() {
        let ident = r2mod.version_ident();
        progress(index, total, ident.full_name());

        let tree = source.stage(&ident, r2mod.source).await?;
        collect_tree(&tree, spec, &mut desired)
            .with_context(|| format!("failed to stage files of {ident}"))?;
    }
    progress(total, total, "");

    info!(payload = desired.payload.len(), "staged published mod set");

    Ok(desired)
}

/// Classifies one staged package tree into payload files and config seeds.
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
        if spec.is_gale_internal(&relative) {
            continue;
        }

        // Config-dir files inside packages are server-owned territory:
        // the running server generates or migrates them, and only an
        // explicit config push writes configs. They are never payload.
        if spec.is_config(&relative) {
            continue;
        }

        if !spec.deploys(&relative, false) {
            continue;
        }

        let file = staged_file(entry.path(), &relative)?;
        desired.payload.insert(relative, file);
    }

    Ok(())
}

fn staged_file(path: &Path, relative: &DeployPathBuf) -> Result<StagedFile> {
    let metadata = path
        .metadata()
        .with_context(|| format!("failed to read staged file {}", path.display()))?;
    let hash = util::fs::checksum(path)
        .with_context(|| format!("failed to hash {relative}"))?
        .to_hex()
        .to_string();

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

    let path = DeployPathBuf::new(value)?;
    ensure!(
        !path.as_str().is_empty(),
        "staged file produced an empty deploy path"
    );
    Ok(path)
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
            export::{ConfigPath, ModRevision, R2Mod},
            server::{plan::Publication, spec::DeploymentSpec},
            sync::archive::ValidatedConfigFile,
        },
        thunderstore::{Backend, PackageIdent, VersionIdent},
    };

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
        let mods = [R2Mod {
            ident: PackageIdent::from(("Author", "SomeMod")),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }];
        let config: BTreeMap<ConfigPath, ValidatedConfigFile> = BTreeMap::new();
        let publication = Publication {
            revision: Utc::now(),
            mods_revision: ModRevision::from_hash(blake3::hash(b"rev")),
            mods: &mods,
            config: &config,
        };

        let desired = stage_publication(&publication, &FailSource, &spec(), false, |_, _, _| {})
            .await
            .unwrap();

        assert!(desired.payload.is_empty());
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
        // server-owned territory — the running server generates or
        // migrates them — so they must not enter the deployment set even
        // when the payload is staged for a mods deployment.
        let package = tempfile::tempdir().unwrap();
        let config_dir = package.path().join("BepInEx/config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("default.cfg"), b"package-default").unwrap();
        let plugin_dir = package.path().join("BepInEx/plugins/Mod");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("Mod.dll"), b"dll").unwrap();

        let mods = [R2Mod {
            ident: PackageIdent::from(("Author", "Mod")),
            version: semver::Version::new(1, 0, 0).into(),
            enabled: true,
            source: Backend::Thunderstore,
        }];
        let config: BTreeMap<ConfigPath, ValidatedConfigFile> = BTreeMap::new();
        let publication = Publication {
            revision: Utc::now(),
            mods_revision: ModRevision::from_hash(blake3::hash(b"rev")),
            mods: &mods,
            config: &config,
        };
        let source = StaticSource(package.path().to_path_buf());

        let desired = stage_publication(&publication, &source, &spec(), true, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(desired.payload.len(), 1);
        assert!(
            desired
                .payload
                .keys()
                .all(|path| path.as_str() == "BepInEx/plugins/Mod/Mod.dll"),
            "the bundled config must not enter the deployment set"
        );
    }
}
