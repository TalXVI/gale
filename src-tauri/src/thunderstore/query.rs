use std::{cmp::Ordering, collections::HashSet, time::Duration};

use eyre::Result;
use indexmap::IndexMap;
use internment::Intern;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tracing::info;

use super::{
    Backend, BorrowedMod, DeduplicatedMod, Thunderstore,
    models::{FrontendMod, FrontendModKind, FrontendVersion},
};
use crate::{
    profile::{LocalMod, ModKey, ModManager},
    state::ManagerExt,
    util,
};

pub fn setup(app: &AppHandle) {
    tauri::async_runtime::spawn(query_loop(app.clone()));
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum SortBy {
    Newest,
    Name,
    Author,
    LastUpdated,
    Downloads,
    Rating,
    InstallDate,
    Custom,
    DiskSpace,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum SortOrder {
    Ascending,
    Descending,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueryModsArgs {
    pub max_count: Option<usize>,
    pub search_term: Option<String>,
    pub include_categories: HashSet<Intern<String>>,
    pub exclude_categories: HashSet<Intern<String>>,
    pub include_nsfw: bool,
    pub include_deprecated: bool,
    pub include_disabled: bool,
    pub include_enabled: bool,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
}

pub async fn query_loop(app: AppHandle) -> Result<()> {
    const INTERVAL: Duration = Duration::from_millis(500);

    loop {
        {
            let mut thunderstore = app.lock_thunderstore();

            if let Some(args) = &thunderstore.current_query {
                let manager = app.lock_manager();

                let mods = thunderstore.query_mods(args, &manager).collect_vec();
                app.emit_buffered("mod_query_result", &mods);

                if thunderstore.packages_fetched(&app, manager.active_game) {
                    info!("all packages fetched, pausing query loop");
                    thunderstore.current_query = None;
                }
            }
        };

        tokio::time::sleep(INTERVAL).await;
    }
}

/// Abstracts logic needed for `query_mods`, allowing it to be reused
/// for both Thunderstore and profile querying.
pub trait Queryable {
    /// The package's full name, including the author.
    fn full_name(&self) -> &str;

    /// The packages latest version.
    fn version(&self) -> Option<semver::Version>;

    /// Whether the package should be included in the given query.
    fn matches(&self, args: &QueryModsArgs) -> bool;

    /// Whether the package should rank higher than `other` in the given query.
    fn cmp(&self, other: &Self, args: &QueryModsArgs) -> Ordering;

    /// The backend the package belongs to.
    fn backend(&self) -> Backend;

    /// A longer description of the package.
    fn description(&self) -> Option<&str> {
        None
    }

    /// Whether the package is deprecated.
    fn is_deprecated(&self) -> bool {
        false
    }
}

impl Queryable for BorrowedMod<'_> {
    fn full_name(&self) -> &str {
        self.package.ident.as_str()
    }

    fn version(&self) -> Option<semver::Version> {
        Some(self.package.latest().parsed_version())
    }

    fn description(&self) -> Option<&str> {
        Some(&self.version.description)
    }

    fn matches(&self, args: &QueryModsArgs) -> bool {
        let pkg = self.package;

        if !args.include_nsfw && pkg.has_nsfw_content
            || !args.include_deprecated && pkg.is_deprecated
        {
            return false;
        }

        if !args.include_categories.is_empty()
            && args.include_categories.is_disjoint(&pkg.categories)
        {
            return false;
        }

        if !args.exclude_categories.is_empty()
            && !args.exclude_categories.is_disjoint(&pkg.categories)
        {
            return false;
        }

        true
    }

    fn cmp(&self, other: &Self, args: &QueryModsArgs) -> Ordering {
        let (a, b) = (self.package, other.package);

        b.is_pinned.cmp(&a.is_pinned).then_with(|| {
            let order = match args.sort_by {
                SortBy::Newest => a.date_created.cmp(&b.date_created),
                SortBy::Name => util::cmp_ignore_case(a.name(), b.name()),
                SortBy::Author => util::cmp_ignore_case(&a.ident, &b.ident),
                SortBy::LastUpdated => a.date_updated.cmp(&b.date_updated),
                SortBy::Downloads => a.total_downloads().cmp(&b.total_downloads()),
                SortBy::Rating => a.rating_score.cmp(&b.rating_score),
                SortBy::DiskSpace => self.version.file_size.cmp(&other.version.file_size),
                SortBy::InstallDate => Ordering::Equal,
                SortBy::Custom => Ordering::Equal,
            };

            match args.sort_order {
                SortOrder::Ascending => order,
                SortOrder::Descending => order.reverse(),
            }
        })
    }

    fn backend(&self) -> Backend {
        self.package.backend
    }
}

impl From<BorrowedMod<'_>> for FrontendMod {
    fn from(borrowed_mod: BorrowedMod<'_>) -> FrontendMod {
        let pkg = borrowed_mod.package;
        let vers = pkg.get_version(borrowed_mod.version.uuid).unwrap();
        FrontendMod {
            name: pkg.name().to_owned(),
            description: Some(vers.description.to_string()),
            version: Some(vers.parsed_version()),
            categories: Some(
                pkg.categories
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
            ),
            author: Some(pkg.owner().to_owned()),
            rating: Some(pkg.rating_score),
            downloads: Some(pkg.total_downloads()),
            file_size: vers.file_size,
            website_url: if vers.website_url.is_empty() {
                None
            } else {
                Some(vers.website_url.to_string())
            },
            donate_url: pkg.donation_link.clone(),
            dependencies: Some(vers.dependencies.clone()),
            suggestions: Some(vers.suggestions.clone()),
            is_pinned: pkg.is_pinned,
            is_deprecated: pkg.is_deprecated,
            contains_nsfw: pkg.has_nsfw_content,
            uuid: pkg.uuid,
            version_uuid: vers.uuid,
            last_updated: Some(pkg.versions[0].date_created.to_rfc3339()),
            versions: pkg
                .versions
                .iter()
                .map(|v| FrontendVersion {
                    name: v.parsed_version(),
                    uuid: v.uuid,
                })
                .collect(),
            kind: FrontendModKind::Remote,
            icon: None,
            backend: pkg.backend,
        }
    }
}

impl From<LocalMod> for FrontendMod {
    fn from(local_mod: LocalMod) -> FrontendMod {
        let LocalMod {
            name,
            description,
            version,
            file_size,
            uuid,
            dependencies,
            icon,
            ..
        } = local_mod;

        FrontendMod {
            name,
            description,
            version,
            file_size,
            uuid,
            dependencies,
            icon,
            kind: FrontendModKind::Local,
            ..Default::default()
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModListQueryItem {
    pub is_installed: bool,
    pub data: DeduplicatedMod<FrontendMod>,
}

impl Thunderstore {
    /// Sorts and filters `mods` according to `args` and converts the
    /// results to [`FrontendMod`].
    pub(super) fn query_mods(
        &self,
        args: &QueryModsArgs,
        manager: &ModManager,
    ) -> impl Iterator<Item = ModListQueryItem> {
        // The halves can have different uuids, so the hidden uuids are resolved
        // to names on both backends first. Filtering before deduplication keeps
        // hidden mods from consuming slots in a `max_count` window.
        let hidden_keys: HashSet<ModKey> = manager
            .hidden_mods
            .iter()
            .filter_map(|uuid| {
                [Backend::Thunderstore, Backend::Hexium]
                    .into_iter()
                    .find_map(|backend| self.get_package(*uuid, backend).ok())
            })
            .map(|package| ModKey::new(package.full_name()))
            .collect();

        let results = query_mods(
            args,
            self.latest()
                .filter(|m| !hidden_keys.contains(&ModKey::new(m.full_name()))),
        );

        let profile = manager.active_profile();

        deduplicate_results(results, args.max_count.unwrap_or(usize::MAX)).map(|deduplicated| {
            let is_installed = [
                deduplicated.thunderstore.as_ref(),
                deduplicated.hexium.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|m| profile.has_name(&ModKey::new(m.full_name())));
            let deduplicated = deduplicated.map(FrontendMod::from);

            ModListQueryItem {
                is_installed,
                data: deduplicated,
            }
        })
    }
}

/// Sorts and filters `mods` according to `args`.
/// Does **not** limit the number of results according to `args.max_count`.
/// Limiting is instead left up to the caller to handle.
pub fn query_mods<'a, T, I>(args: &QueryModsArgs, mods: I) -> Vec<T>
where
    T: Queryable + 'a,
    I: Iterator<Item = T> + 'a,
{
    let search_terms = args.search_term.as_ref().map(|str| {
        let description_query = str.to_lowercase().trim().to_owned();
        let package_query = description_query.replace(' ', "_");
        // search for packages with underscores and descriptions with spaces
        (description_query, package_query)
    });

    let mut results = mods
        .filter(|queryable| {
            if let Some((description_query, package_query)) = &search_terms {
                let name_match = queryable.full_name().to_lowercase().contains(package_query);

                let description_match = queryable.description().is_some_and(|description| {
                    description.to_lowercase().contains(description_query)
                });

                if !name_match && !description_match {
                    return false;
                }
            }

            queryable.matches(args)
        })
        .collect_vec();

    results.sort_by(|a, b| a.cmp(b, args));

    results
}

/// Combines query results from multiple backends into a single list of [`DeduplicatedMod`],
/// merging listings that share a name (`owner-name`, ignoring case).
fn deduplicate_results<T>(
    results: impl IntoIterator<Item = T>,
    max_count: usize,
) -> impl Iterator<Item = DeduplicatedMod<T>>
where
    T: Queryable,
{
    use indexmap::map::Entry;

    let mut deduped_mods: IndexMap<ModKey, DeduplicatedMod<T>> = IndexMap::new();

    // We need to loop through all results as packages from different sources
    // might be ranked differently, and we want to make sure we get every duplicate.
    for result in results {
        let accepting_new_mods = deduped_mods.len() < max_count;
        let entry = deduped_mods.entry(ModKey::new(result.full_name()));

        match entry {
            Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().set(result.backend(), result);
            }
            Entry::Vacant(entry) if accepting_new_mods => {
                let mut deduplicated_mod = DeduplicatedMod::default();
                deduplicated_mod.set(result.backend(), result);
                entry.insert(deduplicated_mod);
            }
            Entry::Vacant(_) => (),
        }
    }

    deduped_mods.into_values()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use indexmap::IndexMap;
    use uuid::Uuid;

    use super::*;
    use crate::profile::{
        ManagedGame, Profile,
        install::queue::tests::{catalogs, install_from, listing, profile},
    };

    fn args() -> QueryModsArgs {
        QueryModsArgs {
            max_count: None,
            search_term: None,
            include_categories: Default::default(),
            exclude_categories: Default::default(),
            include_nsfw: true,
            include_deprecated: true,
            include_disabled: true,
            include_enabled: true,
            sort_by: SortBy::Name,
            sort_order: SortOrder::Ascending,
        }
    }

    fn manager_with(profile: Profile) -> ModManager {
        let game = profile.game;
        let managed = ManagedGame {
            id: 1,
            game,
            path: profile.path.clone(),
            active_profile_id: profile.id,
            profiles: vec![profile],
            favorite: false,
        };
        ModManager {
            active_game: game,
            hidden_mods: Default::default(),
            games: HashMap::from([(game, managed)]),
        }
    }

    #[test]
    fn the_name_index_tracks_cache_live_bulk_and_cleared_packages() {
        let mut state = catalogs(&[(Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])])]);

        // found after a cache load, ignoring letter case
        assert_eq!(
            state
                .find_package_by_key(&ModKey::new("example-MOD"), Backend::Thunderstore)
                .map(|package| package.uuid),
            Some(Uuid::from_u128(1))
        );

        // found after a live listing is applied
        state
            .backend_mut(Backend::Thunderstore)
            .insert_live_package(listing("Live", 9, "1.0.0", &[]), Utc::now());
        assert!(
            state
                .find_package_by_key(&ModKey::new("Example-Live"), Backend::Thunderstore)
                .is_some()
        );

        // rebuilt by a bulk replacement, which also forgets removed packages
        state.backend_mut(Backend::Thunderstore).replace_packages(
            IndexMap::from([(Uuid::from_u128(5), listing("Replaced", 5, "1.0.0", &[]))]),
            None,
        );
        assert!(
            state
                .find_package_by_key(&ModKey::new("Example-Replaced"), Backend::Thunderstore)
                .is_some()
        );
        assert!(
            state
                .find_package_by_key(&ModKey::new("Example-Mod"), Backend::Thunderstore)
                .is_none()
        );

        // forgotten after the backend is cleared
        state.backend_mut(Backend::Thunderstore).clear_packages();
        assert!(
            state
                .find_package_by_key(&ModKey::new("Example-Replaced"), Backend::Thunderstore)
                .is_none()
        );
    }

    #[test]
    fn same_name_listings_merge_and_count_as_installed() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.1", &[])]),
        ]);

        // only the hexium copy is installed, under its own uuid
        let profile = profile(vec![install_from(&state, Backend::Hexium, 2)]);
        let manager = manager_with(profile);

        let items: Vec<_> = state.query_mods(&args(), &manager).collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_installed);
        assert!(items[0].data.thunderstore.is_some());
        assert!(items[0].data.hexium.is_some());
    }

    #[test]
    fn hidden_mods_do_not_shorten_a_browse_page() {
        // "Example-Twin" is listed on both backends under different uuids.
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[
                    listing("Twin", 1, "1.0.0", &[]),
                    listing("Alpha", 2, "1.0.0", &[]),
                    listing("Beta", 3, "1.0.0", &[]),
                ],
            ),
            (Backend::Hexium, &[listing("Twin", 4, "1.0.0", &[])]),
        ]);

        // only the hexium uuid is hidden; the name is still excluded
        let mut manager = manager_with(profile(vec![]));
        manager.hidden_mods.insert(Uuid::from_u128(4));

        let items: Vec<_> = state
            .query_mods(
                &QueryModsArgs {
                    max_count: Some(2),
                    ..args()
                },
                &manager,
            )
            .collect();

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| {
            [item.data.thunderstore.as_ref(), item.data.hexium.as_ref()]
                .into_iter()
                .flatten()
                .all(|m| m.name != "Twin")
        }));
    }
}
