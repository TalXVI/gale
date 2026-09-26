//! Per-user Windows notification-area companion for the managed Worker.
//!
//! The SCM service runs as LocalSystem in Session 0 and never owns UI.
//! Gale copies this helper to a versioned directory under LocalAppData,
//! registers that copy in HKCU Run, and passes the bundled Worker path as
//! the update candidate. Running outside Gale's install directory keeps
//! both app updates and Worker binary swaps free of executable locks.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{Context, Result, bail, ensure};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem as NativeMenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use uuid::Uuid;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, ReleaseMutex, SetEvent,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MB_ICONERROR, MB_OK, MSG, MessageBoxW, PM_REMOVE, PeekMessageW,
    TranslateMessage, WM_QUIT,
};
use windows::core::{HSTRING, PCWSTR};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

use super::local::{self, ManagedServiceState, ServiceControlAction};

const TOOLTIP: &str = "Gale Worker";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "GaleWorkerTray";
const INSTANCE_MUTEX: &str = r"Local\GaleWorkerTray";
const SHUTDOWN_EVENT: &str = r"Local\GaleWorkerTrayShutdown";
const SERVICE_POLL: Duration = Duration::from_millis(500);
const UPDATE_POLL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceObservation {
    NotInstalled,
    Stopped,
    StartPending,
    Running,
    StopPending,
    Other,
}

impl From<ManagedServiceState> for ServiceObservation {
    fn from(value: ManagedServiceState) -> Self {
        match value {
            ManagedServiceState::NotInstalled => Self::NotInstalled,
            ManagedServiceState::Stopped => Self::Stopped,
            ManagedServiceState::StartPending => Self::StartPending,
            ManagedServiceState::Running => Self::Running,
            ManagedServiceState::StopPending => Self::StopPending,
            ManagedServiceState::Other => Self::Other,
        }
    }
}

#[derive(Debug, Default)]
pub struct TrayState {
    observation: Option<ServiceObservation>,
}

impl TrayState {
    pub fn observe(&mut self, observation: ServiceObservation) {
        self.observation = Some(observation);
    }

    pub fn icon_visible(&self) -> bool {
        self.observation == Some(ServiceObservation::Running)
    }
}

#[derive(Debug, Clone)]
pub struct UpdatePlan {
    candidate: PathBuf,
    installed: PathBuf,
}

impl UpdatePlan {
    pub fn new(candidate: PathBuf, installed: PathBuf, tray_helper: PathBuf) -> Result<Self> {
        ensure!(
            candidate != tray_helper,
            "the tray helper cannot be used as the Worker update source"
        );
        ensure!(
            installed != tray_helper,
            "the tray helper must not run from the installed Worker path"
        );
        Ok(Self {
            candidate,
            installed,
        })
    }

    fn update_available(&self) -> bool {
        local::binaries_differ(&self.candidate, &self.installed)
    }

    /// Runs the elevated reinstall.
    /// That command waits on `Local\GaleWorkerUpdate`
    /// holding the mutex here would deadlock the child, which blocks until this process released it.
    pub(crate) fn perform(&self) -> Result<()> {
        let log = std::env::temp_dir().join(format!("gale-worker-update-{}.log", Uuid::new_v4()));
        let args = format!("service reinstall --log \"{}\"", log.display());
        let exit = local::run_elevated_worker(&self.candidate, &args)?;
        if exit != 0 {
            let detail = std::fs::read_to_string(&log).unwrap_or_default();
            bail!("Worker update failed (exit {exit}): {}", detail.trim());
        }
        let _ = std::fs::remove_file(log);
        Ok(())
    }
}

struct NamedMutex(HANDLE);

impl NamedMutex {
    fn acquire(name: &str) -> Result<Option<Self>> {
        let name = HSTRING::from(name);
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) }
            .context("failed to create the companion instance guard")?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) }?;
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

struct ShutdownEvent(HANDLE);

impl ShutdownEvent {
    fn create() -> Result<Self> {
        let name = HSTRING::from(SHUTDOWN_EVENT);
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }
            .context("failed to create the companion shutdown event")?;
        Ok(Self(handle))
    }

    fn requested(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_OBJECT_0
    }
}

impl Drop for ShutdownEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn request_shutdown() {
    let name = HSTRING::from(SHUTDOWN_EVENT);
    let Ok(handle) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) })
    else {
        return;
    };
    unsafe {
        let _ = SetEvent(handle);
        let _ = CloseHandle(handle);
    }
}

fn local_root() -> Result<PathBuf> {
    dirs_next::data_local_dir()
        .map(|path| path.join("Gale").join("worker-tray"))
        .ok_or_else(|| eyre::eyre!("cannot resolve LocalAppData for the Worker companion"))
}

fn install_helper_copy(source: &Path, root: &Path) -> Result<(PathBuf, PathBuf)> {
    let bytes = std::fs::read(source).context("failed to read the bundled Worker tray helper")?;
    let hash = blake3::hash(&bytes).to_hex();
    let version_dir = root.join(&hash.as_str()[..16]);
    let installed = version_dir.join(local::TRAY_EXE);
    std::fs::create_dir_all(&version_dir)
        .context("failed to create the Worker companion directory")?;
    // The version directory is named from the source bytes.
    // A partial `installed` file must not count as finished.
    // The next refresh would see it and skip the rewrite forever.
    if std::fs::read(&installed).ok().as_deref() != Some(bytes.as_slice()) {
        let temporary = installed.with_extension("tmp");
        std::fs::write(&temporary, &bytes).context("failed to install the Worker tray helper")?;
        if installed.exists() {
            if let Err(err) = std::fs::remove_file(&installed) {
                let _ = std::fs::remove_file(&temporary);
                return Err(err).context("failed to replace a partial Worker tray helper");
            }
        }
        if let Err(err) = std::fs::rename(&temporary, &installed) {
            let _ = std::fs::remove_file(&temporary);
            return Err(err).context("failed to publish the Worker tray helper");
        }
    }
    Ok((version_dir, installed))
}

fn registration_command(helper: &Path, worker_candidate: &Path) -> String {
    format!(
        "\"{}\" --worker-candidate \"{}\"",
        helper.display(),
        worker_candidate.display()
    )
}

fn registered_command() -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .ok()?
        .get_value(RUN_VALUE)
        .ok()
}

fn write_registration(command: &str) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY)
        .context("failed to open the current user's startup registry key")?;
    key.set_value(RUN_VALUE, &command)
        .context("failed to register the Worker companion at login")
}

fn remove_registration() -> Result<()> {
    let Ok(key) =
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
    else {
        return Ok(());
    };
    match key.delete_value(RUN_VALUE) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("failed to remove the Worker companion startup entry"),
    }
}

/// Copies the helper to LocalAppData, registers it for this user's logon,
/// and starts it now. Versioned directories let Gale replace its bundled
/// helper during app updates without touching a running executable.
pub fn install_for_current_user(source: &Path, worker_candidate: &Path) -> Result<()> {
    ensure!(
        source.is_file(),
        "the bundled Worker tray helper is missing"
    );
    ensure!(
        worker_candidate.is_file(),
        "the bundled Worker update candidate is missing"
    );
    let root = local_root()?;
    let (version_dir, installed) = install_helper_copy(source, &root)?;

    let command = registration_command(&installed, worker_candidate);
    let changed = registered_command().as_deref() != Some(command.as_str());
    write_registration(&command)?;
    if changed {
        request_shutdown();
        wait_for_companion_exit()?;
    }
    Command::new(&installed)
        .arg("--worker-candidate")
        .arg(worker_candidate)
        .spawn()
        .context("failed to start the Worker tray companion")?;

    cleanup_old_versions(&root, &version_dir);
    Ok(())
}

fn cleanup_old_versions(root: &Path, current: &Path) {
    let Ok(entries) = root.read_dir() else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != current && path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Removes this user's logon registration and asks the resident helper to
/// exit, which drops any visible icon before its LocalAppData copy is removed.
pub fn uninstall_for_current_user() -> Result<()> {
    remove_registration()?;
    request_shutdown();
    wait_for_companion_exit()?;
    let root = local_root()?;
    if root.is_dir() {
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("failed to remove {}", root.display()))?;
    }
    Ok(())
}

fn wait_for_companion_exit() -> Result<()> {
    for _ in 0..50 {
        if NamedMutex::acquire(INSTANCE_MUTEX)?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("the Worker tray companion did not exit within 5 seconds")
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Update,
    Restart,
    Stop,
}

struct MenuBindings {
    menu: Menu,
    update: Option<MenuId>,
    restart: MenuId,
    stop: MenuId,
}

impl MenuBindings {
    fn new(update_available: bool) -> Result<Self> {
        let menu = Menu::new();
        let update = update_available.then(|| NativeMenuItem::new("Update", true, None));
        let restart = NativeMenuItem::new("Restart", true, None);
        let stop = NativeMenuItem::new("Stop", true, None);
        if let Some(update) = &update {
            menu.append(update)?;
            menu.append(&PredefinedMenuItem::separator())?;
        }
        menu.append(&restart)?;
        menu.append(&stop)?;
        Ok(Self {
            menu,
            update: update.map(|item| item.id().clone()),
            restart: restart.id().clone(),
            stop: stop.id().clone(),
        })
    }

    fn action(&self, id: &MenuId) -> Option<Action> {
        if self.update.as_ref() == Some(id) {
            Some(Action::Update)
        } else if &self.restart == id {
            Some(Action::Restart)
        } else if &self.stop == id {
            Some(Action::Stop)
        } else {
            None
        }
    }
}

fn gale_icon() -> Result<Icon> {
    let image = image::load_from_memory(include_bytes!("../../icons/icon.png"))?.into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).context("failed to decode the Gale icon")
}

fn pump_messages() -> bool {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return false;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    true
}

fn show_error(message: &str) {
    let message = HSTRING::from(message);
    let title = HSTRING::from(TOOLTIP);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn begin_action(
    action: Action,
    plan: UpdatePlan,
    busy: &Arc<AtomicBool>,
    result: &Arc<Mutex<Option<std::result::Result<(), String>>>>,
) {
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let busy = Arc::clone(busy);
    let result = Arc::clone(result);
    std::thread::spawn(move || {
        let outcome = match action {
            Action::Update => plan.perform(),
            Action::Restart => local::control_service(ServiceControlAction::Restart),
            Action::Stop => local::control_service(ServiceControlAction::Stop),
        }
        .map_err(|err| format!("{err:#}"));
        *result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        busy.store(false, Ordering::Release);
    });
}

fn run(worker_candidate: PathBuf) -> Result<()> {
    let Some(_instance) = NamedMutex::acquire(INSTANCE_MUTEX)? else {
        return Ok(());
    };
    let shutdown = ShutdownEvent::create()?;
    let helper = std::env::current_exe().context("failed to resolve the tray helper path")?;
    let plan = UpdatePlan::new(
        worker_candidate,
        local::root_dir().join(local::WORKER_EXE),
        helper,
    )?;
    let icon = gale_icon()?;
    let mut tray: Option<TrayIcon> = None;
    let mut menu: Option<MenuBindings> = None;
    let mut state = TrayState::default();
    let busy = Arc::new(AtomicBool::new(false));
    let action_result: Arc<Mutex<Option<std::result::Result<(), String>>>> =
        Arc::new(Mutex::new(None));
    let mut last_service_poll = Instant::now() - SERVICE_POLL;
    let mut last_update_poll = Instant::now() - UPDATE_POLL;
    let mut update_available = plan.update_available();

    while !shutdown.requested() {
        if !pump_messages() {
            break;
        }

        if let Some(outcome) = action_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            if let Err(err) = outcome {
                show_error(&err);
            }
            last_service_poll = Instant::now() - SERVICE_POLL;
            last_update_poll = Instant::now() - UPDATE_POLL;
        }

        if last_service_poll.elapsed() >= SERVICE_POLL {
            let observation = local::service_state()
                .map(ServiceObservation::from)
                .unwrap_or(ServiceObservation::Other);
            state.observe(observation);
            if state.icon_visible() && tray.is_none() {
                let bindings = MenuBindings::new(update_available)?;
                tray = Some(
                    TrayIconBuilder::new()
                        .with_tooltip(TOOLTIP)
                        .with_icon(icon.clone())
                        .with_menu(Box::new(bindings.menu.clone()))
                        .with_menu_on_left_click(false)
                        .build()?,
                );
                menu = Some(bindings);
            } else if !state.icon_visible() {
                tray = None;
                menu = None;
            }
            last_service_poll = Instant::now();
        }

        if last_update_poll.elapsed() >= UPDATE_POLL {
            let available = plan.update_available();
            if available != update_available {
                update_available = available;
                if let (Some(tray), Some(_)) = (&tray, &menu) {
                    let bindings = MenuBindings::new(update_available)?;
                    tray.set_menu(Some(Box::new(bindings.menu.clone())));
                    menu = Some(bindings);
                }
            }
            last_update_poll = Instant::now();
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if let Some(action) = menu.as_ref().and_then(|menu| menu.action(&event.id)) {
                begin_action(action, plan.clone(), &busy, &action_result);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(tray);
    Ok(())
}

pub fn run_from_args() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut worker_candidate = None;
    while let Some(arg) = args.next() {
        if arg == "--worker-candidate" {
            worker_candidate = args.next().map(PathBuf::from);
        }
    }
    run(worker_candidate.ok_or_else(|| eyre::eyre!("--worker-candidate is required"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_companion_startup_is_rejected() {
        let first = NamedMutex::acquire(r"Local\GaleWorkerTrayTest").unwrap();
        assert!(first.is_some());
        let second = NamedMutex::acquire(r"Local\GaleWorkerTrayTest").unwrap();
        assert!(second.is_none());
    }

    #[test]
    fn update_plan_rejects_the_tray_binary_as_a_worker_source_or_install() {
        let bundled = PathBuf::from(r"C:\Program Files\Gale\gale-worker.exe");
        let installed = PathBuf::from(r"C:\ProgramData\Gale\worker\gale-worker.exe");
        let helper = PathBuf::from(r"C:\Users\me\AppData\Local\Gale\worker-tray\tray.exe");
        assert!(UpdatePlan::new(helper.clone(), installed, helper.clone()).is_err());
        assert!(UpdatePlan::new(bundled, helper.clone(), helper).is_err());
    }

    #[test]
    fn update_item_tracks_binary_contents_and_disappears_after_install() {
        let root = tempfile::tempdir().unwrap();
        let candidate = root.path().join("bundled.exe");
        let installed = root.path().join("installed.exe");
        let helper = root.path().join("tray.exe");
        std::fs::write(&candidate, b"new worker").unwrap();
        std::fs::write(&installed, b"old worker").unwrap();
        let plan = UpdatePlan::new(candidate.clone(), installed.clone(), helper).unwrap();
        assert!(plan.update_available());
        std::fs::copy(candidate, installed).unwrap();
        assert!(!plan.update_available());
    }

    #[test]
    fn registration_points_at_the_local_helper_and_current_worker_candidate() {
        let helper =
            Path::new(r"C:\Users\me\AppData\Local\Gale\worker-tray\abc\gale-worker-tray.exe");
        let candidate = Path::new(r"C:\Program Files\Gale\gale-worker.exe");
        let command = registration_command(helper, candidate);
        assert!(command.starts_with(&format!("\"{}\"", helper.display())));
        assert!(command.ends_with(&format!("\"{}\"", candidate.display())));
        assert!(!command.contains(r"ProgramData\Gale\worker\gale-worker.exe"));
    }

    #[test]
    fn helper_copy_replaces_a_partial_file_and_leaves_no_temp() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("bundled-tray.exe");
        let contents = b"complete helper";
        std::fs::write(&source, contents).unwrap();
        let dest_root = root.path().join("tray");
        let hash = blake3::hash(contents).to_hex();
        let version_dir = dest_root.join(&hash.as_str()[..16]);
        std::fs::create_dir_all(&version_dir).unwrap();
        let installed = version_dir.join(local::TRAY_EXE);
        std::fs::write(&installed, b"partial").unwrap();

        let (_, published) = install_helper_copy(&source, &dest_root).unwrap();
        assert_eq!(published, installed);
        assert_eq!(std::fs::read(&installed).unwrap(), contents);
        assert!(!installed.with_extension("tmp").exists());

        install_helper_copy(&source, &dest_root).unwrap();
        assert_eq!(std::fs::read(&installed).unwrap(), contents);
        assert!(!installed.with_extension("tmp").exists());
    }
}
