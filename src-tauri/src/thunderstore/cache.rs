use std::{
    fmt::Display,
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use http_cache_reqwest::CacheMode;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use super::{Backend, ModId, PackageListing, Thunderstore};
use crate::{
    game::Game, prefs::Prefs, state::ManagerExt, thunderstore::backend::ThunderstoreBackend, util,
};

#[derive(Debug, Deserialize)]
struct MarkdownResponse {
    markdown: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum MarkdownKind {
    Readme,
    Changelog,
}

impl Display for MarkdownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarkdownKind::Readme => write!(f, "readme"),
            MarkdownKind::Changelog => write!(f, "changelog"),
        }
    }
}

pub async fn get_markdown(
    cache: MarkdownKind,
    mod_id: ModId,
    app: &AppHandle,
) -> Result<Option<String>> {
    let (url, cache_mode) = {
        let thunderstore = app.lock_thunderstore();
        let ident = mod_id.borrow(&thunderstore)?.ident();

        let cache_mode = if mod_id.backend.force_cache_markdown() {
            CacheMode::ForceCache
        } else {
            CacheMode::Default
        };

        (mod_id.backend.markdown_url(ident, cache), cache_mode)
    };

    let response: MarkdownResponse = app
        .http()
        .get(url)
        .with_extension(cache_mode)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(response.markdown)
}

impl ThunderstoreBackend {
    pub fn read_and_insert_cache(&mut self, game: Game, prefs: &Prefs) {
        match get_packages(game, prefs, self.backend) {
            Ok(Some(mods)) => {
                for entry in mods {
                    if let Some(observed_at) = entry.live_observed_at {
                        self.insert_live_package(entry.package, observed_at);
                    } else {
                        self.packages.insert(entry.package.uuid, entry.package);
                    }
                }
            }
            Ok(None) => (),
            Err(err) => warn!("failed to read cache: {}", err),
        }
    }
}

// Keep the array and listing fields readable by older Gale versions.
#[derive(Serialize, Deserialize)]
struct CachedPackage<P = PackageListing> {
    #[serde(flatten)]
    package: P,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live_observed_at: Option<DateTime<Utc>>,
}

fn get_packages(game: Game, prefs: &Prefs, backend: Backend) -> Result<Option<Vec<CachedPackage>>> {
    let start = Instant::now();
    let path = cache_path(game, prefs, backend);

    if !path.exists() {
        info!("no cache file found at {}", path.display());
        return Ok(None);
    }

    let result: Vec<CachedPackage> = util::fs::read_json::<Vec<CachedPackage>>(path)
        .context("failed to deserialize cache")?
        .into_iter()
        .map(|mut entry| {
            entry.package.backend = backend;
            entry
        })
        .collect();

    debug!(
        "read {} packages from cache in {:?}",
        result.len(),
        start.elapsed()
    );

    Ok(Some(result))
}

pub fn write_packages<'a>(
    mut packages: Vec<&'a PackageListing>,
    game: Game,
    prefs: &Prefs,
    thunderstore: &'a Thunderstore,
) -> Result<()> {
    packages.extend(thunderstore.backend(Backend::Thunderstore).live_packages());
    packages.extend(thunderstore.backend(Backend::Hexium).live_packages());
    let mut packages = packages
        .into_iter()
        .unique_by(|p| (p.backend, p.uuid))
        .collect_vec();
    if packages.is_empty() {
        info!("no packages to write to cache");
        return Ok(());
    }

    packages.sort_by_key(|p| p.backend);

    let start = Instant::now();
    for (backend, packages) in &packages.iter().chunk_by(|p| p.backend) {
        let entries = packages
            .map(|package| CachedPackage {
                package: *package,
                live_observed_at: thunderstore.backend(backend).live_observed_at(package.uuid),
            })
            .collect_vec();
        write_cached_packages(&entries, game, prefs, backend)?;
    }

    debug!(
        "wrote {} packages to cache in {:?}",
        packages.len(),
        start.elapsed()
    );

    Ok(())
}

/// Saves live listings before making them visible to installation planning.
pub(crate) fn write_live_packages(
    packages: &[(PackageListing, DateTime<Utc>)],
    game: Game,
    prefs: &Prefs,
) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }
    let backend = Backend::Thunderstore;
    let cached = get_packages(game, prefs, backend)?.unwrap_or_default();
    let mut entries = cached
        .iter()
        .filter(|entry| {
            !packages
                .iter()
                .any(|(package, _)| package.uuid == entry.package.uuid)
        })
        .map(|entry| CachedPackage {
            package: &entry.package,
            live_observed_at: entry.live_observed_at,
        })
        .collect_vec();
    entries.extend(packages.iter().map(|(package, observed_at)| CachedPackage {
        package,
        live_observed_at: Some(*observed_at),
    }));
    write_cached_packages(&entries, game, prefs, backend)
}

fn write_cached_packages<P: Serialize>(
    packages: &[CachedPackage<P>],
    game: Game,
    prefs: &Prefs,
    backend: Backend,
) -> Result<()> {
    let path = cache_path(game, prefs, backend);
    let directory = path
        .parent()
        .ok_or_else(|| eyre::eyre!("mod cache has no parent directory"))?;
    let temporary =
        tempfile::NamedTempFile::new_in(directory).context("failed to create mod cache")?;
    {
        let mut writer = BufWriter::new(temporary.as_file());
        serde_json::to_writer(&mut writer, packages).context("failed to serialize mod cache")?;
        writer.flush().context("failed to flush mod cache")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync mod cache")?;
    temporary
        .persist(&path)
        .context("failed to replace mod cache")?;
    Ok(())
}

fn cache_path(game: Game, prefs: &Prefs, backend: Backend) -> PathBuf {
    prefs
        .data_dir
        .join(&*game.slug)
        .join(format!("{backend}_cache.json"))
}
