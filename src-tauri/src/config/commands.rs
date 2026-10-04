use std::path::Path;

use eyre::{Context, eyre};
use tauri::{AppHandle, command};

use super::{AnyFileKind, frontend};
use crate::{
    profile::{ModManager, Profile, server},
    state::ManagerExt,
    util::cmd::Result,
};

// The config cache is populated for the active profile only, so a request
// pinned to a profile that is no longer active is rejected rather than
// silently applied to the wrong one.
fn active_profile_guarded(manager: &mut ModManager, profile_id: i64) -> Result<&mut Profile> {
    let profile = manager.active_profile_mut();
    if profile.id != profile_id {
        return Err(eyre!("active profile changed while the config editor was open").into());
    }
    Ok(profile)
}

/// [`active_profile_guarded`] for commands that write to the profile's
/// files, which a running dedicated server keeps locked.
fn writable_profile_guarded<'m>(
    manager: &'m mut ModManager,
    profile_id: i64,
    app: &AppHandle,
) -> Result<&'m mut Profile> {
    let profile = active_profile_guarded(manager, profile_id)?;
    server::ensure_profile_unlocked(app, profile.id)?;
    Ok(profile)
}

#[command]
pub fn get_config_files(profile_id: i64, app: AppHandle) -> Result<Vec<frontend::File>> {
    let mut manager = app.lock_manager();
    let profile = active_profile_guarded(&mut manager, profile_id)?;

    profile.refresh_config();

    Ok(profile.config_cache.to_frontend())
}

#[command]
pub fn set_config_entry(
    file: &Path,
    section: &str,
    entry: &str,
    value: frontend::Value,
    profile_id: i64,
    app: AppHandle,
) -> Result<()> {
    let mut manager = app.lock_manager();

    let profile = writable_profile_guarded(&mut manager, profile_id, &app)?;
    let file = profile.config_cache.find_file(file)?;

    match &mut file.kind {
        AnyFileKind::BepInEx(file) => file.find_entry(section, entry)?.set(value),
        AnyFileKind::GDWeave(file) => file.set(entry, value),
        _ => return Err(eyre!("unsupported for this format").into()),
    }?;

    file.write(&profile.path).context("failed to write file")?;
    Ok(())
}

#[command]
pub fn reset_config_entry(
    file: &Path,
    section: &str,
    entry: &str,
    profile_id: i64,
    app: AppHandle,
) -> Result<frontend::Value> {
    let mut manager = app.lock_manager();

    let profile = writable_profile_guarded(&mut manager, profile_id, &app)?;
    let file = profile.config_cache.find_file(file)?;

    let value = match &mut file.kind {
        AnyFileKind::BepInEx(file) => file.find_entry(section, entry)?.reset(),
        _ => return Err(eyre!("unsupported for this format").into()),
    }?;

    file.write(&profile.path).context("failed to write file")?;
    Ok(value)
}

#[command]
pub fn reset_config_file(file: &Path, profile_id: i64, app: AppHandle) -> Result<()> {
    let mut manager = app.lock_manager();

    let profile = writable_profile_guarded(&mut manager, profile_id, &app)?;
    let file = profile.config_cache.find_file(file)?;

    match &mut file.kind {
        AnyFileKind::BepInEx(file) => file.reset_all()?,
        _ => return Err(eyre!("unsupported for this format").into()),
    }

    file.write(&profile.path).context("failed to write file")?;
    Ok(())
}

#[command]
pub fn open_config_file(file: &Path, profile_id: i64, app: AppHandle) -> Result<()> {
    let mut manager = app.lock_manager();

    let profile = active_profile_guarded(&mut manager, profile_id)?;
    let path = profile.path.join(file);
    open::that_detached(&path)
        .with_context(|| format!("failed to open config file at {}", path.display()))?;

    Ok(())
}

#[command]
pub fn delete_config_file(file: &Path, profile_id: i64, app: AppHandle) -> Result<()> {
    let mut manager = app.lock_manager();

    let profile = writable_profile_guarded(&mut manager, profile_id, &app)?;

    let Some(index) = profile
        .config_cache
        .0
        .iter()
        .position(|f| f.relative_path == file)
    else {
        return Ok(()); // ignore if the file is not in the list
    };

    profile.config_cache.0.remove(index);

    let path = profile.path.join(file);
    trash::delete(path).context("failed to move file to recycle bin")?;

    Ok(())
}
