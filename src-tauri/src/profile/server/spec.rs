use eyre::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::{
    paths::{DeployPath, DeployPathBuf},
    state,
};
use crate::game::mod_loader::ModLoader;

/// Top-level names inside a profile directory that belong to Gale itself and
/// are never deployed.
const PROFILE_INTERNAL_NAMES: &[&str] = &["profile.json", "mods.yml", "snapshots", "_state"];

/// Remote names managed by Gale inside config directories. They are not
/// configs and are never treated as managed config files.
const GALE_INTERNAL_NAMES: &[&str] = &[
    state::FILE_NAME,
    state::LEGACY_FILE_NAME,
    state::LEASE_DIR_NAME,
];

/// How a profile should be laid out on a dedicated server, derived from the
/// game's mod loader. This is the single place where mod-loader-specific
/// deployment policy lives; the rest of the deployment pipeline is generic.
///
/// Two distinct areas are described:
///
/// - [`Self::payload_dirs`] hold the mod payload. Their remote contents are
///   kept in exact sync with the published mod set: files missing from the
///   publication are removed, but only within Gale's recorded ownership.
/// - [`Self::config_dirs`] hold server configuration. They are
///   policy-governed: nothing inside them is ever removed by a deployment,
///   and writes happen only through config synchronization rules.
pub struct DeploymentSpec {
    /// The top-level directory the mod loader installs into inside the
    /// profile, e.g. `BepInEx`. Managed hosts may expose this directory
    /// directly at the remote root instead of the profile root.
    pub mirror_root: DeployPathBuf,

    /// Mod payload directories synchronized exactly with the publication.
    pub payload_dirs: Vec<DeployPathBuf>,

    /// Config directories governed by selective config synchronization.
    /// Their contents are never removed and only written through config
    /// apply rules (selected/policies) or package-default seeding.
    pub config_dirs: Vec<DeployPathBuf>,

    /// Deploy paths that identify the mod loader's own payload in the
    /// profile. Gale-owned files matching these are the loader deployment.
    loader_owned_globs: GlobSet,

    /// Deploy paths whose remote presence marks the loader installation as
    /// managed by the host rather than by Gale.
    pub loader_markers: Vec<DeployPathBuf>,

    /// Where the deployment state record lives, inside a config directory
    /// so restricted hosts can store it too.
    pub state_path: DeployPathBuf,

    /// Where the lease directory is created to coordinate executors.
    pub lease_dir: DeployPathBuf,

    /// The previous deployment manifest's location, read once to adopt its
    /// ownership records into the new state format.
    pub legacy_manifest_path: DeployPathBuf,

    /// Deploy paths that are never uploaded: generated caches, logs and
    /// package metadata that is only meaningful locally.
    exclude_globs: GlobSet,
}

impl DeploymentSpec {
    /// Builds the deployment spec for a mod loader, or fails if the loader
    /// has no dedicated-server deployment support.
    pub fn for_loader(mod_loader: &ModLoader<'static>) -> Result<Self> {
        let mirror_dirs = mod_loader
            .server_mirror_dirs()
            .ok_or_else(|| {
                eyre::eyre!(
                    "dedicated server deployment is not supported for {}",
                    mod_loader.as_str()
                )
            })?
            .iter()
            .map(DeployPathBuf::new)
            .collect::<Result<Vec<_>>>()?;

        let mirror_root = mirror_dirs
            .first()
            .and_then(|dir| dir.segments().next().map(str::to_owned))
            .map(DeployPathBuf::new)
            .ok_or_else(|| eyre::eyre!("mod loader has no mirror directories"))??;

        let config_dirs = mod_loader
            .mod_config_dirs()
            .iter()
            .map(DeployPathBuf::new)
            .collect::<Result<Vec<_>>>()?;

        let config_dir = config_dirs
            .first()
            .ok_or_else(|| eyre::eyre!("mod loader has no config directory"))?
            .clone();

        let payload_dirs = mirror_dirs
            .into_iter()
            .filter(|dir| {
                !config_dirs
                    .iter()
                    .any(|config| dir.is_within(config) || config.is_within(dir))
            })
            .collect::<Vec<_>>();

        let loader_owned_globs = build_globs(&[
            // Loader payload installed at the profile root. Restricted to
            // well-known mod loader files; mods live in mirror dirs and never
            // decide loader ownership.
            "bepinex/core/**",
            "doorstop_config.ini",
            "winhttp.dll",
            ".doorstop_version",
            "doorstop_libs/**",
            "dotnet/**",
        ])?;

        let loader_markers = [DeployPathBuf::new("BepInEx/core")?].into_iter().collect();

        // Patterns are matched against lowercased deploy paths.
        let exclude_globs = build_globs(&[
            "bepinex/cache/**",
            "bepinex/dumpedassemblies/**",
            "bepinex/interop/**",
            "renderer/bepinex/cache/**",
            "renderer/bepinex/dumpedassemblies/**",
            "renderer/bepinex/interop/**",
            "**/logoutput.log",
            // Package metadata mods only need locally. `manifest.json` is
            // deliberately not excluded: some mods read it at runtime.
            "bepinex/plugins/*/{readme.md,changelog.md,icon.png}",
            "renderer/bepinex/plugins/*/{readme.md,changelog.md,icon.png}",
        ])?;

        Ok(Self {
            mirror_root,
            payload_dirs,
            config_dirs,
            loader_owned_globs,
            loader_markers,
            state_path: config_dir.join(state::FILE_NAME)?,
            lease_dir: config_dir.join(state::LEASE_DIR_NAME)?,
            legacy_manifest_path: config_dir.join(state::LEGACY_FILE_NAME)?,
            exclude_globs,
        })
    }

    /// Strips the mirror root prefix from `path`, e.g. `BepInEx/plugins/x`
    /// becomes `plugins/x` on a restricted host. The mirror root itself maps
    /// to the remote root.
    pub fn strip_mirror_root(&self, path: &DeployPath) -> Option<DeployPathBuf> {
        path.strip_prefix(self.mirror_root.as_str())
    }

    /// Whether recorded Gale-owned files include a loader deployment,
    /// meaning Gale (and not the host) owns the loader files on the server.
    pub fn owns_loader<'a>(&self, mut paths: impl Iterator<Item = &'a DeployPath>) -> bool {
        paths.any(|path| self.is_loader_owned(path))
    }

    /// Whether `path` is part of the mod loader's own payload.
    pub fn is_loader_owned(&self, path: &DeployPath) -> bool {
        self.loader_owned_globs
            .is_match(path.as_str().to_lowercase())
    }

    /// Whether `path` is covered by the synchronized payload directories.
    pub fn is_mirrored(&self, path: &DeployPath) -> bool {
        self.payload_dirs.iter().any(|dir| dir.is_ancestor_of(path))
    }

    /// Whether `path` sits inside a policy-governed config directory.
    pub fn is_config(&self, path: &DeployPath) -> bool {
        self.config_dirs.iter().any(|dir| path.is_within(dir))
    }

    /// Whether `path` is one of Gale's own bookkeeping files.
    pub fn is_gale_internal(&self, path: &DeployPath) -> bool {
        GALE_INTERNAL_NAMES.contains(&path.file_name())
            || path.is_within(&self.lease_dir)
            || self.is_internal(path)
    }

    /// Whether a file from the published mod set may be deployed to `path`.
    ///
    /// `host_managed` means the remote host provides the loader itself, in
    /// which case loader-owned paths outside the payload dirs are left
    /// alone. Config-directory paths are never payload-deployed: they are
    /// written through config rules instead.
    pub fn deploys(&self, path: &DeployPath, host_managed: bool) -> bool {
        if self.is_internal(path) || self.is_excluded(path) || self.is_config(path) {
            return false;
        }

        if self.is_mirrored(path) {
            return true;
        }

        self.is_loader_owned(path) && !host_managed
    }

    /// Whether a package-bundled file under a config dir may seed a server
    /// config when absent (never overwriting existing content).
    pub fn is_config_seed(&self, path: &DeployPath) -> bool {
        self.is_config(path) && !self.is_gale_internal(path) && !self.is_excluded(path)
    }

    /// Whether a recorded state entry may authorize deleting the remote file
    /// at `path`. This is the boundary that keeps a tampered state file from
    /// widening deletion authority: only payload-scope paths qualify.
    pub fn valid_owned_path(&self, path: &DeployPath) -> bool {
        if self.is_internal(path) || self.is_excluded(path) || self.is_config(path) {
            return false;
        }

        self.is_mirrored(path) || self.is_loader_owned(path)
    }

    /// Whether `path` may be removed during deployment given loader
    /// ownership. Deletion is bounded to payload scope; under a host-managed
    /// loader only mirrored payload paths are eligible.
    pub fn owns_for_removal(&self, path: &DeployPath, host_managed: bool) -> bool {
        self.valid_owned_path(path) && (!host_managed || self.is_mirrored(path))
    }

    fn is_internal(&self, path: &DeployPath) -> bool {
        path.segments()
            .next()
            .is_some_and(|name| PROFILE_INTERNAL_NAMES.contains(&name))
    }

    fn is_excluded(&self, path: &DeployPath) -> bool {
        self.exclude_globs.is_match(path.as_str().to_lowercase())
    }
}

fn build_globs(patterns: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).literal_separator(true).build()?);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bepinex() -> DeploymentSpec {
        DeploymentSpec::for_loader(&ModLoader {
            package_name: None,
            file_target: None,
            kind: crate::game::mod_loader::ModLoaderKind::BepInEx {
                extra_subdirs: Vec::new(),
            },
        })
        .unwrap()
    }

    fn path(value: &str) -> DeployPathBuf {
        DeployPathBuf::new(value).unwrap()
    }

    #[test]
    fn payload_dirs_exclude_config_dirs() {
        let spec = bepinex();

        assert!(spec.is_mirrored(&path("BepInEx/plugins/Mod/Mod.dll")));
        assert!(spec.is_mirrored(&path("BepInEx/patchers/x.dll")));
        assert!(spec.is_mirrored(&path("BepInEx/monomod/x.mm.dll")));
        assert!(!spec.is_mirrored(&path("BepInEx/config/mod.cfg")));
        assert!(!spec.is_mirrored(&path("BepInEx/core/bepinex.dll")));
        assert!(!spec.is_mirrored(&path("doorstop_config.ini")));
        assert!(spec.is_config(&path("BepInEx/config/mod.cfg")));
        assert_eq!(spec.mirror_root.as_str(), "BepInEx");
        assert_eq!(
            spec.state_path.as_str(),
            "BepInEx/config/.gale-server-state.json"
        );
    }

    #[test]
    fn excludes_generated_and_internal_paths() {
        let spec = bepinex();

        for excluded in [
            "BepInEx/cache/x",
            "BepInEx/DumpedAssemblies/Game.dll",
            "BepInEx/interop/x.dll",
            "BepInEx/LogOutput.log",
            "BepInEx/plugins/Mod/README.md",
            "BepInEx/plugins/Mod/icon.png",
            "profile.json",
            "mods.yml",
            "_state/Author-Mod.json",
            "BepInEx/config/.gale-server-state.json",
            "BepInEx/config/.gale-server-manifest.json",
        ] {
            assert!(!spec.deploys(&path(excluded), false), "deployed {excluded}");
            assert!(!spec.valid_owned_path(&path(excluded)), "owned {excluded}");
        }

        for deployed in [
            "BepInEx/plugins/Mod/Mod.dll",
            "BepInEx/plugins/Mod/manifest.json",
            "doorstop_config.ini",
            "BepInEx/core/bepinex.dll",
        ] {
            assert!(spec.deploys(&path(deployed), false), "skipped {deployed}");
        }
    }

    #[test]
    fn config_paths_are_never_payload_owned() {
        let spec = bepinex();

        // A state file claiming ownership of a config path must not authorize
        // its deletion: configs live under policy, not payload mirroring.
        assert!(!spec.valid_owned_path(&path("BepInEx/config/mod.cfg")));
        assert!(!spec.deploys(&path("BepInEx/config/mod.cfg"), false));
        assert!(spec.is_config_seed(&path("BepInEx/config/mod.cfg")));
    }

    #[test]
    fn managed_hosts_only_receive_mirrored_payload() {
        let spec = bepinex();

        assert!(spec.deploys(&path("BepInEx/plugins/Mod/Mod.dll"), true));
        assert!(!spec.deploys(&path("BepInEx/core/bepinex.dll"), true));
        assert!(!spec.deploys(&path("doorstop_config.ini"), true));
        assert!(!spec.owns_for_removal(&path("BepInEx/core/bepinex.dll"), true));
        assert!(spec.owns_for_removal(&path("BepInEx/core/bepinex.dll"), false));
    }

    #[test]
    fn detects_loader_ownership_from_state_files() {
        let spec = bepinex();
        let files = [path("BepInEx/core/bepinex.dll")];

        assert!(spec.owns_loader(files.iter().map(|p| p.as_path())));
        assert!(!spec.owns_loader(std::iter::empty()));
    }

    #[test]
    fn rejects_loaders_without_deployment_support() {
        let melon = ModLoader {
            package_name: None,
            file_target: None,
            kind: crate::game::mod_loader::ModLoaderKind::MelonLoader {
                extra_subdirs: Vec::new(),
            },
        };

        assert!(DeploymentSpec::for_loader(&melon).is_err());
    }
}
