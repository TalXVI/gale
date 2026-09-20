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
            let tmp = parent.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&tmp)?;
            let result = self.extract(bytes, ident, &tmp);
            match result {
                Ok(()) => {
                    if dest.exists() {
                        std::fs::remove_dir_all(&tmp)?;
                    } else {
                        std::fs::rename(&tmp, &dest).or_else(|_| {
                            util::fs::copy_dir(
                                &tmp,
                                &dest,
                                util::fs::Overwrite::Yes,
                                util::fs::UseLinks::No,
                            )
                            .and_then(|_| std::fs::remove_dir_all(&tmp).map_err(eyre::Report::from))
                        })?;
                    }
                    Ok(dest)
                }
                Err(error) => {
                    std::fs::remove_dir_all(&tmp).ok();
                    Err(error)
                }
            }
        })
    }
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

    info!(
        payload = desired.payload.len(),
        defaults = desired.package_defaults.len(),
        "staged published mod set"
    );

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

        if spec.is_config(&relative) {
            if spec.is_config_seed(&relative) {
                let file = staged_file(entry.path(), &relative)?;
                desired.package_defaults.insert(relative, file);
            }
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
        assert!(desired.package_defaults.is_empty());
    }
}
