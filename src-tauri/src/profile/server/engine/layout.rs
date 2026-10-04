//! How deploy paths map onto the remote host's directory layout.

use eyre::{Context, Result};

use crate::profile::server::{
    paths::{DeployPath, RemotePath, RemotePathBuf},
    plan::RemoteLayout,
    remote::RemoteOps,
    spec::DeploymentSpec,
    state::ServerDeploymentState,
};

/// Maps deploy paths to absolute remote paths for the detected layout.
pub(super) struct RemoteMapper {
    pub(super) spec: DeploymentSpec,
    pub(super) base: RemotePathBuf,
    pub(super) layout: RemoteLayout,
}

impl RemoteMapper {
    pub(super) fn remote_path(&self, deploy: &DeployPath) -> RemotePathBuf {
        match self.layout {
            RemoteLayout::Standard => self.base.join(deploy),
            RemoteLayout::MirrorRoot => self
                .spec
                .strip_mirror_root(deploy)
                .map(|relative| self.base.join(&relative))
                .unwrap_or_else(|| self.base.join(deploy)),
        }
    }
}

/// Detects the remote layout. A host that exposes the loader's contents
/// directly shows stripped mirror dirs (`plugins`, `config`, ...) at the
/// remote root; that presence is authoritative over a stray `BepInEx`
/// directory, which earlier deployments could have created accidentally
/// and which must not flip a restricted host back to Standard.
pub(super) fn detect_layout(
    ops: &mut dyn RemoteOps,
    spec: &DeploymentSpec,
    base: &RemotePath,
) -> Result<RemoteLayout> {
    for dir in spec.payload_dirs.iter().chain(spec.config_dirs.iter()) {
        let Some(at_root) = spec.strip_mirror_root(dir) else {
            continue;
        };
        if ops
            .is_dir(&base.join(&at_root))
            .context("failed to check for a restricted server layout")?
        {
            return Ok(RemoteLayout::MirrorRoot);
        }
    }

    Ok(RemoteLayout::Standard)
}

/// Whether the host manages the mod loader itself. Restricted hosts always
/// do. On standard layouts the loader marker files decide, unless Gale
/// recorded a loader deployment, because files Gale uploaded stay Gale's.
pub(super) fn detect_host_managed(
    ops: &mut dyn RemoteOps,
    mapper: &RemoteMapper,
    state: &ServerDeploymentState,
) -> Result<bool> {
    if mapper.layout == RemoteLayout::MirrorRoot {
        return Ok(true);
    }

    if state
        .files
        .keys()
        .any(|path| mapper.spec.is_loader_owned(path))
    {
        return Ok(false);
    }

    for marker in &mapper.spec.loader_markers {
        if ops
            .is_dir(&mapper.remote_path(marker))
            .context("failed to check for a server-managed mod loader installation")?
        {
            return Ok(true);
        }
    }

    Ok(false)
}
