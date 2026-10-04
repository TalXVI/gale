use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use eyre::{Context, Result};
use tauri::AppHandle;
use tracing::{info, warn};

use crate::{
    state::ManagerExt,
    thunderstore::{Backend, FromBackend, VersionIdent, query::Queryable},
    util,
};

pub(crate) fn path(root: &Path, ident: &VersionIdent, backend: Backend) -> PathBuf {
    root.join(backend.to_string())
        .join(ident.full_name())
        .join(ident.version())
}

pub(super) fn clear(path: PathBuf) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(&path).context("failed to delete cache directory")?;
        fs::create_dir_all(path).context("failed to recreate cache directory")?;
    }

    Ok(())
}

pub(super) fn prepare_soft_clear(app: AppHandle) -> Result<Vec<PathBuf>> {
    let prefs = app.lock_prefs();
    let manager = app.lock_manager();
    let thunderstore = app.lock_thunderstore();
    let root = prefs.cache_dir();

    let installed_mods = manager
        .active_game()
        .installed_mods(&thunderstore)
        .map(|borrowed| path(&root, borrowed.ident(), borrowed.backend()))
        .collect::<HashSet<_>>();

    unused_versions(&root, &installed_mods, |name| {
        thunderstore.find_package(name, FromBackend::Any).is_ok()
    })
}

fn unused_versions(
    root: &Path,
    installed: &HashSet<PathBuf>,
    is_known_package: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut to_remove = Vec::new();
    for entry in fs::read_dir(root).context("failed to read cache directory")? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let entry_path = entry.path();
        if util::fs::file_name_owned(&entry_path)
            .parse::<Backend>()
            .is_ok()
        {
            for package in fs::read_dir(&entry_path)? {
                let package = package?;
                let package_path = package.path();
                if package.file_type()?.is_dir()
                    && is_known_package(&util::fs::file_name_owned(&package_path))
                {
                    collect_unused_versions(&package_path, installed, &mut to_remove)?;
                }
            }
        } else if is_known_package(&util::fs::file_name_owned(&entry_path)) {
            // Legacy entries have no source identity and cannot be reused.
            collect_unused_versions(&entry_path, &HashSet::new(), &mut to_remove)?;
        }
    }
    Ok(to_remove)
}

fn collect_unused_versions(
    package: &Path,
    installed: &HashSet<PathBuf>,
    to_remove: &mut Vec<PathBuf>,
) -> Result<()> {
    for version in fs::read_dir(package)
        .with_context(|| format!("failed to read cache for {}", package.display()))?
    {
        let version = version?;
        let path = version.path();
        if version.file_type()?.is_dir() && !installed.contains(&path) {
            to_remove.push(path);
        }
    }
    Ok(())
}

fn is_empty_dir(path: &Path) -> bool {
    path.read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
}

pub(super) fn do_soft_clear(paths: Vec<PathBuf>) -> Result<()> {
    let count = paths.len();

    for path in paths {
        let package_dir = path.parent();

        fs::remove_dir_all(&path)?;

        // Clearing the last version of a mod leaves its package directory behind
        let Some(dir) = package_dir else {
            continue;
        };

        if !is_empty_dir(dir) {
            continue;
        }

        if let Err(err) = fs::remove_dir(dir) {
            warn!("failed to remove empty cache directory {dir:?}: {err}");
        }
    }

    info!("cleared {} mods from cache", count);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clear_retains_only_installed_sources_and_preserves_other_games() {
        let root = tempfile::tempdir().unwrap();
        let ident = VersionIdent::from(("Author", "Mod", "1.0.0"));
        let thunderstore = path(root.path(), &ident, Backend::Thunderstore);
        let hexium = path(root.path(), &ident, Backend::Hexium);
        let legacy = root.path().join("Author-Mod/1.0.0");
        let other_game = root.path().join("hexium/Other-Game/1.0.0");
        for dir in [&thunderstore, &hexium, &legacy, &other_game] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("mod.dll"), b"payload").unwrap();
        }
        let installed = HashSet::from([thunderstore.clone()]);
        let paths = unused_versions(root.path(), &installed, |name| name == "Author-Mod").unwrap();
        assert_eq!(
            HashSet::from_iter(paths.clone()),
            HashSet::from([hexium.clone(), legacy.clone()])
        );

        do_soft_clear(paths).unwrap();
        assert!(thunderstore.join("mod.dll").exists());
        assert!(other_game.join("mod.dll").exists());
        assert!(!hexium.exists());
        assert!(!legacy.exists());
    }
}
