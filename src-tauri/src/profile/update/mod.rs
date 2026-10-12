use chrono::{DateTime, Utc};
use eyre::OptionExt;
use itertools::Itertools;
use serde::Serialize;
use tauri::AppHandle;
use uuid::Uuid;

use super::install::{InstallOptions, ModInstall};
use crate::{
    profile::{
        ModKey, Profile, Result, ThunderstoreMod,
        install::{InstallResultExt, queue::InstallQueueLock},
    },
    state::ManagerExt,
    thunderstore::{BorrowedMod, FromBackend, ModId, Thunderstore},
};

pub mod commands;

/// The other site's listing of an installed mod, and why it is worth showing.
pub struct SourceAlternative<'a> {
    pub target: BorrowedMod<'a>,
    pub reason: Option<AlternativeReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AlternativeReason {
    /// The other site has a newer version under a different package uuid.
    Newer,
    /// The installed listing is deprecated and the other site's listing is not.
    DeprecatedHere,
    /// The installed version is no longer listed on its site.
    Delisted,
    /// The installed listing is deprecated and the other site's is not, but its version is older.
    OlderButNotDeprecated,
}

impl AlternativeReason {
    /// Whether the UI asks the user to switch. Informational reasons never prompt.
    pub fn is_prompted(self) -> bool {
        !matches!(self, Self::OlderButNotDeprecated)
    }
}

pub struct AvailableUpdate<'a> {
    pub enabled: bool,
    pub index: usize,
    pub install_time: DateTime<Utc>,
    pub current: BorrowedMod<'a>,
    pub latest: BorrowedMod<'a>,
}

impl From<AvailableUpdate<'_>> for ModInstall {
    fn from(value: AvailableUpdate<'_>) -> Self {
        ModInstall::new(value.latest)
            .with_state(value.enabled)
            .with_index(value.index)
            .with_time(value.install_time)
            .replacing(value.current.package.uuid)
    }
}

impl Profile {
    pub fn is_update_ignored(&self, update: &AvailableUpdate<'_>) -> bool {
        let latest = &update.latest;

        self.ignored_version_updates.contains(&latest.version.uuid)
            || self.ignored_package_updates.contains(&latest.package.uuid)
    }

    /// Finds the other site's listing of an installed Thunderstore mod.
    ///
    /// The other site is searched by package uuid first, then by name.
    pub fn source_alternative<'a>(
        &self,
        entry: &ThunderstoreMod,
        thunderstore: &'a Thunderstore,
    ) -> Option<SourceAlternative<'a>> {
        let other = entry.id.backend.other();
        let other_package = thunderstore
            .get_package(entry.id.package_uuid, other)
            .ok()
            .or_else(|| {
                thunderstore.find_package_by_key(&ModKey::new(entry.ident.full_name()), other)
            })?;

        let installed_version = entry.ident.version().parse::<semver::Version>().ok()?;
        let target_version = if installed_version.pre.is_empty() {
            other_package.latest_released()
        } else {
            other_package.latest()
        };
        let target = BorrowedMod {
            package: other_package,
            version: target_version,
        };

        let Ok(installed) = entry.id.borrow(thunderstore) else {
            return Some(SourceAlternative {
                target,
                reason: Some(AlternativeReason::Delisted),
            });
        };

        let target_version = target.version.parsed_version();
        let same_uuid = other_package.uuid == entry.id.package_uuid;
        let ignored = self.ignored_version_updates.contains(&target.version.uuid)
            || self.ignored_package_updates.contains(&other_package.uuid);
        let installed_deprecated = installed.package.is_deprecated;
        let other_deprecated = other_package.is_deprecated;

        let reason = if target_version > installed_version
            && !same_uuid
            && (!other_deprecated || installed_deprecated)
            && !ignored
        {
            Some(AlternativeReason::Newer)
        } else if installed_deprecated && !other_deprecated {
            if target_version >= installed_version {
                Some(AlternativeReason::DeprecatedHere)
            } else {
                Some(AlternativeReason::OlderButNotDeprecated)
            }
        } else {
            None
        };

        Some(SourceAlternative { target, reason })
    }

    /// The install that moves the installed entry with `install`'s name onto `install`'s version.
    pub fn version_change(&self, install: ModInstall) -> Result<ModInstall> {
        let (index, entry) = self
            .find_by_key(&install.key())
            .ok_or_eyre("mod not found in profile")?;

        Ok(install
            .with_state(entry.enabled)
            .with_index(index)
            .with_time(entry.install_time)
            .replacing(entry.uuid()))
    }

    pub fn check_update<'a>(
        &'a self,
        uuid: Uuid,
        thunderstore: &'a Thunderstore,
        install_queue: &InstallQueueLock,
    ) -> Result<Option<AvailableUpdate<'a>>> {
        if install_queue.has_mod(uuid, self.id) {
            return Ok(None); // a new version of this mod is installing or pending
        }

        let index = self.index_of(uuid)?;
        let profile_mod = &self.mods[index];

        let Some((ts_mod, _)) = profile_mod.as_thunderstore() else {
            return Ok(None); // local mods can't be updated
        };

        let Ok(current) = ts_mod.id.borrow(thunderstore) else {
            return Ok(None); // ignore missing mods
        };

        let package = thunderstore.get_package(
            current.package.uuid,
            FromBackend::PreferIfEqual(current.package.backend),
        )?;
        let current_version = current.version.parsed_version();

        let latest = BorrowedMod {
            package,
            version: if current_version.pre.is_empty() {
                package.latest_released()
            } else {
                package.latest()
            },
        };

        if current_version >= latest.version.parsed_version() {
            return Ok(None);
        }

        Ok(Some(AvailableUpdate {
            index,
            enabled: profile_mod.enabled,
            install_time: profile_mod.install_time,
            current,
            latest,
        }))
    }
}

pub async fn change_version(mod_id: ModId, app: &AppHandle) -> Result<()> {
    let (profile_id, install) = {
        let manager = app.lock_manager();
        let thunderstore = app.lock_thunderstore();

        let profile = manager.active_profile();
        let install = profile.version_change(ModInstall::try_from_id(mod_id, &thunderstore)?)?;

        (profile.id, install)
    };

    install_updates(vec![install], profile_id, app).await
}

pub async fn update_mods(uuids: Vec<Uuid>, respect_ignored: bool, app: &AppHandle) -> Result<()> {
    let (profile_id, installs) = {
        let mut manager = app.lock_manager();
        let thunderstore = app.lock_thunderstore();
        let install_queue = app.install_queue().lock();

        let profile = manager.active_profile_mut();

        let installs = uuids
            .into_iter()
            .filter_map(|uuid| {
                profile
                    .check_update(uuid, &thunderstore, &install_queue)
                    .transpose()
            })
            .filter_ok(|update| !respect_ignored || !profile.is_update_ignored(update))
            .map_ok(ModInstall::from)
            .collect::<Result<Vec<_>>>()?;

        (profile.id, installs)
    };

    install_updates(installs, profile_id, app).await
}

async fn install_updates(
    installs: Vec<ModInstall>,
    profile_id: i64,
    app: &AppHandle,
) -> Result<()> {
    app.install_queue()
        .install_with_deps(
            installs,
            profile_id,
            InstallOptions::default().cancel_individually(),
            app,
        )?
        .await
        .ignore_cancel()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use crate::{
        profile::install::queue::tests::{
            catalogs, install, install_from, listing, listing_versions, profile, test_queue,
        },
        thunderstore::{Backend, PackageListing},
    };

    use super::*;

    fn entry(profile: &Profile, index: usize) -> &ThunderstoreMod {
        profile.mods[index].as_thunderstore().unwrap().0
    }

    fn deprecated(mut package: PackageListing) -> PackageListing {
        package.is_deprecated = true;
        package
    }

    /// `Example-Mod` on Thunderstore and Hexium under different package uuids.
    fn dual_site(ts: &[&str], hx: &[&str]) -> Thunderstore {
        catalogs(&[
            (Backend::Thunderstore, &[listing_versions("Mod", 1, ts)]),
            (Backend::Hexium, &[listing_versions("Mod", 2, hx)]),
        ])
    }

    #[test]
    fn a_name_matched_other_site_listing_is_a_newer_alternative() {
        let state = dual_site(&["1.0.0"], &["1.0.1"]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.reason, Some(AlternativeReason::Newer));
        assert_eq!(alternative.target.package.uuid, Uuid::from_u128(2));
        assert_eq!(alternative.target.version.version(), "1.0.1");
    }

    #[test]
    fn a_same_uuid_cross_site_update_is_not_an_alternative() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (Backend::Hexium, &[listing("Mod", 1, "1.0.1", &[])]),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();
        assert_eq!(alternative.reason, None);

        // the same-uuid case stays a regular cross-backend update
        let queue = test_queue();
        let update = profile
            .check_update(Uuid::from_u128(1), &state, &queue.lock())
            .unwrap()
            .unwrap();
        assert_eq!(update.latest.package.backend, Backend::Hexium);
        assert_eq!(update.latest.version.version(), "1.0.1");
    }

    #[test]
    fn an_otherwise_deprecated_listing_is_a_deprecated_here_alternative() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[deprecated(listing("Mod", 1, "1.0.0", &[]))],
            ),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.0", &[])]),
        ]);
        let deprecated_profile = profile(vec![install(&state, 1)]);

        let alternative = deprecated_profile
            .source_alternative(entry(&deprecated_profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.reason, Some(AlternativeReason::DeprecatedHere));
        assert_eq!(alternative.target.package.uuid, Uuid::from_u128(2));

        // the same applies when both sites share the package uuid
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[deprecated(listing("Mod", 1, "1.0.0", &[]))],
            ),
            (Backend::Hexium, &[listing("Mod", 1, "1.0.0", &[])]),
        ]);
        let shared_uuid_profile = profile(vec![install(&state, 1)]);

        let alternative = shared_uuid_profile
            .source_alternative(entry(&shared_uuid_profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.reason, Some(AlternativeReason::DeprecatedHere));
    }

    #[test]
    fn an_older_undeprecated_listing_is_informational_only() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[deprecated(listing("Mod", 1, "1.0.1", &[]))],
            ),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.0", &[])]),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();

        assert_eq!(
            alternative.reason,
            Some(AlternativeReason::OlderButNotDeprecated)
        );
        assert!(!alternative.reason.unwrap().is_prompted());
    }

    #[test]
    fn a_deprecated_other_site_listing_is_no_upgrade() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (
                Backend::Hexium,
                &[deprecated(listing("Mod", 2, "1.0.1", &[]))],
            ),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.reason, None);
    }

    #[test]
    fn a_newer_prerelease_does_not_prompt_a_stable_install() {
        let state = dual_site(&["1.0.0"], &["1.5.0-beta.1", "0.9.0"]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.target.version.version(), "0.9.0");
        assert_eq!(alternative.reason, None);
    }

    #[test]
    fn the_other_site_listing_is_found_despite_letter_case() {
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.0", &[])]),
            (Backend::Hexium, &[listing("MOD", 2, "1.0.1", &[])]),
        ]);
        let profile = profile(vec![install(&state, 1)]);

        let alternative = profile
            .source_alternative(entry(&profile, 0), &state)
            .unwrap();

        assert_eq!(alternative.reason, Some(AlternativeReason::Newer));
        assert_eq!(alternative.target.package.uuid, Uuid::from_u128(2));
    }

    #[test]
    fn ignored_updates_suppress_the_newer_reason() {
        let state = dual_site(&["1.0.0"], &["1.0.1"]);

        let mut by_package = profile(vec![install(&state, 1)]);
        by_package
            .ignored_package_updates
            .insert(Uuid::from_u128(2));

        let alternative = by_package
            .source_alternative(entry(&by_package, 0), &state)
            .unwrap();
        assert_eq!(alternative.reason, None);

        let mut by_version = profile(vec![install(&state, 1)]);
        // the hexium 1.0.1 version uuid is 2 * 100 + 1
        by_version
            .ignored_version_updates
            .insert(Uuid::from_u128(201));

        let alternative = by_version
            .source_alternative(entry(&by_version, 0), &state)
            .unwrap();
        assert_eq!(alternative.reason, None);
    }

    #[test]
    fn a_version_change_to_the_other_sites_listing_replaces_the_installed_entry() {
        let state = catalogs(&[
            (
                Backend::Thunderstore,
                &[
                    listing("Other", 3, "1.0.0", &[]),
                    listing("Mod", 1, "1.0.0", &[]),
                ],
            ),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.1", &[])]),
        ]);

        let installed_at = "2024-02-03T04:05:06Z".parse::<DateTime<Utc>>().unwrap();
        let mut profile = profile(vec![install(&state, 3), install(&state, 1)]);
        profile.mods[1].enabled = false;
        profile.mods[1].install_time = installed_at;

        let changed = profile
            .version_change(install_from(&state, Backend::Hexium, 2))
            .unwrap();

        assert_eq!(changed.replaces(), Some(Uuid::from_u128(1)));
        assert_eq!(changed.mod_id().package_uuid, Uuid::from_u128(2));
        assert_eq!(changed.index(), Some(1));
        assert!(!changed.enabled());
        assert_eq!(changed.install_time(), Some(installed_at));
    }
}
