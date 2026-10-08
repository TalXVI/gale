use crate::{
    game::Game,
    thunderstore::{BorrowedMod, PackageIdent, PackageListing, VersionIdent, cache::MarkdownKind},
};
use chrono::{DateTime, Utc};
use eyre::eyre;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    str::FromStr,
};
use uuid::Uuid;

#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, Ord, PartialOrd, PartialEq, Eq, Hash, Default,
)]
pub enum Backend {
    #[default]
    Thunderstore,
    Hexium,
}

impl Display for Backend {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Thunderstore => "thunderstore",
            Backend::Hexium => "hexium",
        })
    }
}

impl FromStr for Backend {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "thunderstore" => Ok(Self::Thunderstore),
            "hexium" => Ok(Self::Hexium),
            _ => Err(()),
        }
    }
}

impl Backend {
    pub fn other(self) -> Self {
        match self {
            Backend::Thunderstore => Backend::Hexium,
            Backend::Hexium => Backend::Thunderstore,
        }
    }

    pub fn index_url(self, game: Game) -> Option<String> {
        if game.backends.contains(&self) {
            Some(match self {
                Backend::Thunderstore => format!(
                    "https://thunderstore.io/c/{}/api/v1/package-listing-index/",
                    game.slug
                ),
                Backend::Hexium => format!(
                    "https://{}.hexium.gg/api/v1/package-listing-index/",
                    game.slug,
                ),
            })
        } else {
            None
        }
    }

    /// Returns the live package listing endpoint when the backend supports it.
    pub(crate) fn package_detail_url(self, game: Game, uuid: Uuid) -> Option<String> {
        match self {
            Backend::Thunderstore if game.backends.contains(&self) => Some(format!(
                "https://thunderstore.io/c/{}/api/v1/package/{uuid}/",
                game.slug
            )),
            _ => None,
        }
    }

    pub fn force_cache_markdown(self) -> bool {
        matches!(self, Backend::Thunderstore)
    }

    pub fn markdown_url(self, ident: &VersionIdent, cache: MarkdownKind) -> String {
        match self {
            Backend::Thunderstore => format!(
                "https://thunderstore.io/api/experimental/package/{}/{}/{}/{}/",
                ident.owner(),
                ident.name(),
                ident.version(),
                cache
            ),
            Backend::Hexium => format!(
                "https://hexium.gg/api/experimental/package/{}/{}/{}/{}/",
                ident.owner(),
                ident.name(),
                ident.version(),
                cache
            ),
        }
    }

    pub fn owner_url(self, owner: &str, game: Game) -> String {
        match self {
            Backend::Thunderstore => {
                format!("https://thunderstore.io/c/{}/p/{}/", game.slug, owner)
            }
            Backend::Hexium => format!("https://{}.hexium.gg/teams/{}", game.slug, owner),
        }
    }

    pub fn mod_url(self, package: &PackageIdent, game: Game) -> String {
        match self {
            Backend::Thunderstore => {
                format!(
                    "https://thunderstore.io/c/{}/p/{}/{}/",
                    game.slug,
                    package.name(),
                    package.name()
                )
            }
            Backend::Hexium => format!(
                "https://{}.hexium.gg/mods/{}/{}",
                game.slug,
                package.name(),
                package.name()
            ),
        }
    }

    pub fn download_url(self, version: &VersionIdent) -> String {
        match self {
            Backend::Thunderstore => format!(
                "https://thunderstore.io/package/download/{}/{}/{}",
                version.owner(),
                version.name(),
                version.version()
            ),
            Backend::Hexium => format!(
                "https://cdn.hexium.gg/uploads/{}/{}/{}.zip",
                version.owner(),
                version.name(),
                version.version()
            ),
        }
    }

    pub fn profile_import(self, key: &str) -> String {
        match self {
            Backend::Thunderstore => {
                format!("https://thunderstore.io/api/experimental/legacyprofile/get/{key}/")
            }
            Backend::Hexium => {
                format!("https://hexium.gg/api/experimental/legacyprofile/get/{key}/")
            }
        }
    }

    pub fn profile_export(self) -> &'static str {
        match self {
            Backend::Thunderstore => {
                "https://thunderstore.io/api/experimental/legacyprofile/create/"
            }
            Backend::Hexium => "https://hexium.gg/api/experimental/legacyprofile/create/",
        }
    }

    pub fn category_url(&self, game: Game) -> String {
        match self {
            Backend::Thunderstore => format!(
                "https://thunderstore.io/api/experimental/community/{}/category/",
                game.slug
            ),
            Backend::Hexium => format!(
                "https://hexium.gg/api/experimental/community/{}/category/",
                game.slug
            ),
        }
    }

    pub fn modpack_upload_base_url(self) -> &'static str {
        match self {
            Backend::Thunderstore => "https://thunderstore.io/api/experimental",
            Backend::Hexium => "https://hexium.gg/api/experimental",
        }
    }
}

/// Registry for all Thunderstore-like mods for the active game (Hexium, Thunderstore).
pub struct ThunderstoreBackend {
    /// Whether packages have been succesfully fetched at least one since
    /// the last call to [`crate::thunderstore::Thunderstore::switch_game`].
    pub(super) packages_fetched: bool,
    /// Whether a [`fetch_mods`] task is currently running.
    is_fetching: bool,
    // IndexMap is not used for ordering here, but for fast iteration,
    // since we iterate over all mods when resolving identifiers and querying.
    pub(super) packages: IndexMap<Uuid, PackageListing>,
    live_packages: HashMap<Uuid, DateTime<Utc>>,
    pub(super) backend: Backend,
}

impl ThunderstoreBackend {
    pub fn new(backend: Backend) -> Self {
        Self {
            packages_fetched: false,
            is_fetching: false,
            packages: IndexMap::new(),
            live_packages: HashMap::new(),
            backend,
        }
    }

    /// Whether packages have been succesfully fetched at least one since
    /// the last call to [`crate::thunderstore::Thunderstore::switch_game`].
    pub fn packages_fetched(&self) -> bool {
        self.packages_fetched
    }

    /// Inserts a catalog listing without overwriting a live pull with stale data.
    pub(super) fn insert_package(
        &mut self,
        package: PackageListing,
        generated_at: Option<DateTime<Utc>>,
    ) {
        if let Some(observed_at) = self.live_packages.get(&package.uuid)
            && self.packages.get(&package.uuid).is_some_and(|current| {
                catalog_precedes_live(current, &package, *observed_at, generated_at)
            })
        {
            return;
        }

        self.live_packages.remove(&package.uuid);
        self.packages.insert(package.uuid, package);
    }

    pub(super) fn insert_live_package(
        &mut self,
        package: PackageListing,
        observed_at: DateTime<Utc>,
    ) {
        self.live_packages.insert(package.uuid, observed_at);
        self.packages.insert(package.uuid, package);
    }

    pub(super) fn live_observed_at(&self, uuid: Uuid) -> Option<DateTime<Utc>> {
        self.live_packages.get(&uuid).copied()
    }

    pub(super) fn live_packages(&self) -> impl Iterator<Item = &PackageListing> {
        self.packages
            .values()
            .filter(|package| self.live_packages.contains_key(&package.uuid))
    }

    /// Keeps live pulls until the bulk catalog catches up or removes the listing.
    pub(super) fn replace_packages(
        &mut self,
        packages: IndexMap<Uuid, PackageListing>,
        generated_at: Option<DateTime<Utc>>,
    ) {
        for (uuid, current) in std::mem::replace(&mut self.packages, packages) {
            if let Some(observed_at) = self.live_packages.get(&uuid)
                && let Some(incoming) = self.packages.get_mut(&uuid)
                && catalog_precedes_live(&current, incoming, *observed_at, generated_at)
            {
                *incoming = current;
            } else if let Some(observed_at) = self.live_packages.get(&uuid)
                && !self.packages.contains_key(&uuid)
                && generated_at.is_some_and(|generated| generated < *observed_at)
            {
                self.packages.insert(uuid, current);
            } else {
                self.live_packages.remove(&uuid);
            }
        }
    }

    /// Returns an iterator over the latest versions of every package.
    pub fn latest(&self) -> impl Iterator<Item = BorrowedMod<'_>> {
        self.packages.values().map(move |package| BorrowedMod {
            package,
            version: package.latest_released(),
        })
    }

    pub fn get_package(&self, uuid: Uuid) -> eyre::Result<&PackageListing> {
        self.packages
            .get(&uuid)
            .ok_or_else(|| eyre!("package with id {uuid} not found"))
    }

    /// Finds a package with the given `full_name` (formatted as `owner-name`).
    pub fn find_package(&self, full_name: &str) -> eyre::Result<&PackageListing> {
        self.packages
            .values()
            .find(|package| package.ident.as_str() == full_name)
            .ok_or_else(|| eyre!("package {full_name} not found"))
    }

    pub fn get_mod(&self, package_uuid: Uuid, version_uuid: Uuid) -> eyre::Result<BorrowedMod<'_>> {
        let package = self.get_package(package_uuid)?;
        let version = package.get_version(version_uuid).ok_or_else(|| {
            eyre!(
                "version with id {version_uuid} not found in package {}",
                package.ident
            )
        })?;

        Ok((package, version).into())
    }

    pub fn find_ident(&self, ident: &VersionIdent) -> eyre::Result<BorrowedMod<'_>> {
        self.find_mod(ident.owner(), ident.name(), ident.version())
    }

    pub fn find_mod<'a>(
        &'a self,
        owner: &str,
        name: &str,
        version: &str,
    ) -> eyre::Result<BorrowedMod<'a>> {
        let package = self
            .packages
            .values()
            .find(|package| package.owner() == owner && package.name() == name)
            .ok_or_else(|| eyre!("package {}-{} not found", owner, name))?;

        let version = package.get_version_with_num(version).ok_or_else(|| {
            eyre!(
                "version {} not found in package {}-{}",
                version,
                owner,
                name
            )
        })?;

        Ok((package, version).into())
    }

    /// Clear the package map.
    pub fn clear_packages(&mut self) {
        self.is_fetching = false;
        self.packages_fetched = false;
        self.packages = IndexMap::new();
        self.live_packages.clear();
    }
}

fn catalog_precedes_live(
    live: &PackageListing,
    catalog: &PackageListing,
    observed_at: DateTime<Utc>,
    generated_at: Option<DateTime<Utc>>,
) -> bool {
    match live.date_updated.cmp(&catalog.date_updated) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        // Withdrawals and moderation do not advance the publication timestamp.
        std::cmp::Ordering::Equal => generated_at.is_some_and(|generated| generated < observed_at),
    }
}
