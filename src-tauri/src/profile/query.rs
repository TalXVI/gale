use std::{cmp::Ordering, path::PathBuf};

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use super::{
    Dependant, LocalMod, Profile, ProfileMod, ProfileModKind,
    update::{AlternativeReason, SourceAlternative},
};
use crate::thunderstore::{
    self, Backend, BorrowedMod, FrontendMod, Thunderstore,
    query::{QueryModsArgs, Queryable, SortBy, SortOrder},
};

struct QueryableProfileMod<'a> {
    enabled: bool,
    install_time: DateTime<Utc>,
    kind: QueryableProfileModKind<'a>,
    index: usize,
}

enum QueryableProfileModKind<'a> {
    Local(&'a LocalMod),
    Thunderstore(BorrowedMod<'a>),
}

impl<'a> QueryableProfileMod<'a> {
    fn create(
        profile_mod: &'a ProfileMod,
        index: usize,
        thunderstore: &'a Thunderstore,
    ) -> Result<QueryableProfileMod<'a>> {
        let kind = match &profile_mod.kind {
            ProfileModKind::Local(local) => QueryableProfileModKind::Local(local),
            ProfileModKind::Thunderstore(ts_mod) => {
                let borrow = ts_mod.id.borrow(thunderstore)?;
                QueryableProfileModKind::Thunderstore(borrow)
            }
        };

        Ok(QueryableProfileMod {
            enabled: profile_mod.enabled,
            install_time: profile_mod.install_time,
            kind,
            index,
        })
    }
}

impl Queryable for QueryableProfileMod<'_> {
    fn full_name(&self) -> &str {
        use QueryableProfileModKind as Kind;

        match &self.kind {
            Kind::Local(local) => local.full_name(),
            Kind::Thunderstore(remote) => remote.full_name(),
        }
    }

    fn version(&self) -> Option<semver::Version> {
        use QueryableProfileModKind as Kind;

        match &self.kind {
            Kind::Local(local) => local.version(),
            Kind::Thunderstore(remote) => Some(remote.version.parsed_version()),
        }
    }

    fn matches(&self, args: &QueryModsArgs) -> bool {
        use QueryableProfileModKind as Kind;

        if !args.include_disabled && !self.enabled {
            return false;
        }

        if !args.include_enabled && self.enabled {
            return false;
        }

        match &self.kind {
            Kind::Local(local) => local.matches(args),
            Kind::Thunderstore(remote) => remote.matches(args),
        }
    }

    fn cmp(&self, other: &Self, args: &QueryModsArgs) -> Ordering {
        use QueryableProfileModKind as Kind;

        let overridden = match args.sort_by {
            SortBy::InstallDate => Some(self.install_time.cmp(&other.install_time)),
            SortBy::Custom => Some(self.index.cmp(&other.index)),
            _ => None,
        };

        if let Some(order) = overridden {
            return match args.sort_order {
                SortOrder::Ascending => order,
                SortOrder::Descending => order.reverse(),
            };
        }

        match (&self.kind, &other.kind) {
            (Kind::Thunderstore(a), Kind::Thunderstore(b)) => a.cmp(b, args),
            (Kind::Local(a), Kind::Local(b)) => a.cmp(b, args),
            (Kind::Local(_), _) => Ordering::Less,
            (_, Kind::Local(_)) => Ordering::Greater,
        }
    }

    fn backend(&self) -> thunderstore::Backend {
        use QueryableProfileModKind as Kind;

        match &self.kind {
            Kind::Local(local) => local.backend(),
            Kind::Thunderstore(remote) => remote.backend(),
        }
    }
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FrontendProfileMod {
    pub enabled: bool,
    pub config_file: Option<PathBuf>,
    /// Whether the mod is also available on another backend. If so, the frontend
    /// can display a context option to switch to the other backend.
    pub alternate_backend: Option<AlternateBackend>,
    pub data: FrontendMod,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlternateBackend {
    pub backend: Backend,
    pub package_uuid: Uuid,
    pub latest_version: String,
    pub latest_version_uuid: Uuid,
    pub reason: Option<AlternativeReason>,
}

impl From<SourceAlternative<'_>> for AlternateBackend {
    fn from(alternative: SourceAlternative<'_>) -> Self {
        let SourceAlternative { target, reason } = alternative;
        let latest = target.version;

        Self {
            backend: target.package.backend,
            package_uuid: target.package.uuid,
            latest_version: latest.version().to_string(),
            latest_version_uuid: latest.uuid,
            reason,
        }
    }
}

/// An installed mod that no longer resolves in the catalog.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnknownMod {
    #[serde(flatten)]
    pub dependant: Dependant,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternative: Option<AlternateBackend>,
}

impl Profile {
    pub(super) fn icon_assets(&self) -> Result<Vec<PathBuf>> {
        let mut icons = Vec::new();
        for (local, _) in self.local_mods() {
            let Some(icon) = &local.icon else { continue };
            if icon.file_name().is_none_or(|name| name != "icon.png") {
                continue;
            }
            let root = self.path.canonicalize()?;
            for path in [icon.clone(), icon.with_extension("png.old")] {
                let canonical = match path.canonicalize() {
                    Ok(path) => path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if canonical.starts_with(&root)
                    && canonical.file_name() == path.file_name()
                    && canonical.is_file()
                {
                    icons.push(canonical);
                }
            }
        }
        Ok(icons)
    }

    pub(super) fn query_mods(
        &self,
        args: &QueryModsArgs,
        thunderstore: &Thunderstore,
    ) -> (Vec<FrontendProfileMod>, Vec<UnknownMod>) {
        let mut unknown = Vec::new();

        let mods = self
            .mods
            .iter()
            .enumerate()
            .filter_map(|(index, profile_mod)| {
                if let Ok(queryable) = QueryableProfileMod::create(profile_mod, index, thunderstore)
                {
                    Some(queryable)
                } else {
                    warn!(
                        "unknown mod: {} while querying {}",
                        profile_mod.ident(),
                        self.name
                    );
                    let alternative = self.alternative_of(profile_mod, thunderstore);
                    unknown.push(UnknownMod {
                        dependant: Dependant::from(profile_mod),
                        alternative,
                    });
                    None
                }
            });

        let query_result = thunderstore::query::query_mods(args, mods)
            .into_iter()
            .take(args.max_count.unwrap_or(usize::MAX))
            .map(|queryable| {
                let index = queryable.index;
                let (data, uuid, alternate_backend) = match queryable.kind {
                    QueryableProfileModKind::Local(local) => {
                        (FrontendMod::from(local.clone()), local.uuid, None)
                    }
                    QueryableProfileModKind::Thunderstore(remote) => (
                        FrontendMod::from(remote),
                        remote.package.uuid,
                        self.alternative_of(&self.mods[index], thunderstore),
                    ),
                };

                FrontendProfileMod {
                    data,
                    enabled: queryable.enabled,
                    alternate_backend,
                    config_file: self.linked_config.get(&uuid).cloned(),
                }
            })
            .collect();

        (query_result, unknown)
    }

    fn alternative_of(
        &self,
        profile_mod: &ProfileMod,
        thunderstore: &Thunderstore,
    ) -> Option<AlternateBackend> {
        let (entry, _) = profile_mod.as_thunderstore()?;

        self.source_alternative(entry, thunderstore)
            .map(AlternateBackend::from)
    }
}

impl Queryable for LocalMod {
    fn full_name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Option<semver::Version> {
        self.version.clone()
    }

    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn matches(&self, _args: &QueryModsArgs) -> bool {
        true
    }

    fn cmp(&self, other: &Self, args: &QueryModsArgs) -> Ordering {
        let order = match args.sort_by {
            SortBy::Name => other.name.cmp(&self.name),
            SortBy::Author => match (&other.author, &self.author) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            },
            _ => Ordering::Equal,
        };

        match args.sort_order {
            SortOrder::Ascending => order,
            SortOrder::Descending => order.reverse(),
        }
    }

    fn backend(&self) -> thunderstore::Backend {
        thunderstore::Backend::Thunderstore
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::install::queue::tests::{
        catalog, catalogs, install, listing, profile, profile_at,
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

    #[test]
    fn security_icon_whitelist_selects_only_icons_inside_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let profile_dir = dir.path().join("profile");
        let icon = profile_dir.join("BepInEx/plugins/Local/icon.png");
        std::fs::create_dir_all(icon.parent().unwrap()).unwrap();
        std::fs::write(&icon, b"fixture-icon").unwrap();
        std::fs::write(icon.with_extension("png.old"), b"fixture-disabled-icon").unwrap();
        let private = profile_dir.join("private.cfg");
        std::fs::write(&private, b"fixture-private-config").unwrap();
        let outside = dir.path().join("icon.png");
        std::fs::write(&outside, b"fixture-outside").unwrap();
        let mut profile = profile_at(profile_dir.clone(), Vec::new());
        for icon_path in [icon.clone(), private.clone(), outside.clone()] {
            profile.mods.push(ProfileMod::new_local(LocalMod {
                icon: Some(icon_path),
                ..Default::default()
            }));
        }
        let allowed = profile.icon_assets().unwrap();
        assert!(allowed.contains(&icon.canonicalize().unwrap()));
        assert!(allowed.contains(&icon.with_extension("png.old").canonicalize().unwrap()));
        assert!(
            !allowed.contains(&private.canonicalize().unwrap()),
            "profile secrets must not be exposed as assets"
        );
        assert!(
            !allowed.contains(&outside.canonicalize().unwrap()),
            "an icon path outside the profile must not expand the scope"
        );
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn unknown_mod_serializes_flat_with_its_alternative() {
        // the installed 1.0.0 is no longer in the Thunderstore catalog
        let state = catalogs(&[
            (Backend::Thunderstore, &[listing("Mod", 1, "1.0.1", &[])]),
            (Backend::Hexium, &[listing("Mod", 2, "1.0.1", &[])]),
        ]);
        let previous = catalog(&[listing("Mod", 1, "1.0.0", &[])]);
        let profile = profile(vec![install(&previous, 1)]);

        let (mods, unknown) = profile.query_mods(&args(), &state);

        assert!(mods.is_empty());
        assert_eq!(unknown.len(), 1);

        let value = serde_json::to_value(&unknown[0]).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "fullName": "Example-Mod-1.0.0",
                "uuid": "00000000-0000-0000-0000-000000000001",
                "backend": "Thunderstore",
                "alternative": {
                    "backend": "Hexium",
                    "packageUuid": "00000000-0000-0000-0000-000000000002",
                    "latestVersion": "1.0.1",
                    "latestVersionUuid": "00000000-0000-0000-0000-0000000000c9",
                    "reason": "delisted"
                }
            })
        );
    }
}
