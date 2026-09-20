use std::{path::PathBuf, sync::Arc, time::Duration};

use eyre::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{game::Game, state::ManagerExt};

pub type SharedChild = Arc<Mutex<tokio::process::Child>>;

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct ServerRuntime {
    running: Option<RunningServer>,
}

pub struct RunningServer {
    profile_id: i64,
    game: Game,
    server_dir: PathBuf,
    pid: u32,
    child: SharedChild,
    /// Termination was requested but not yet confirmed. The entry stays
    /// registered because the profile must remain locked while the
    /// process may still be alive.
    stopping: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerStatus {
    Stopped,

    Running {
        profile_id: i64,
        game_slug: String,
        pid: u32,
        server_dir: PathBuf,
        /// A stop was requested. The process still counts as running
        /// until its exit is confirmed.
        stopping: bool,
    },
}

impl ServerRuntime {
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn is_profile_locked(&self, profile_id: i64) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| running.profile_id == profile_id)
    }

    pub fn status(&self) -> ServerStatus {
        match &self.running {
            Some(running) => ServerStatus::Running {
                profile_id: running.profile_id,
                game_slug: running.game.slug.to_string(),
                pid: running.pid,
                server_dir: running.server_dir.clone(),
                stopping: running.stopping,
            },

            None => ServerStatus::Stopped,
        }
    }

    pub fn server_dir(&self) -> Option<PathBuf> {
        self.running
            .as_ref()
            .map(|running| running.server_dir.clone())
    }

    pub fn register(
        &mut self,
        profile_id: i64,
        game: Game,
        server_dir: PathBuf,
        pid: u32,
        child: SharedChild,
    ) -> Result<ServerStatus> {
        eyre::ensure!(
            self.running.is_none(),
            "a Gale-managed dedicated server is already running"
        );

        self.running = Some(RunningServer {
            profile_id,
            game,
            server_dir,
            pid,
            child,
            stopping: false,
        });

        Ok(self.status())
    }

    /// An old watcher must not clear a newer server.
    pub fn clear_if_pid(&mut self, pid: u32) -> bool {
        let matches = self
            .running
            .as_ref()
            .is_some_and(|running| running.pid == pid);

        matches && self.running.take().is_some()
    }

    /// Marks the tracked server as terminating and returns its pid and
    /// process handle. The registration stays in place for the whole
    /// termination. The profile stays locked and `is_running` keeps
    /// blocking new launches until the process is confirmed dead, so a
    /// failed or slow kill cannot expose the profile or let a replacement
    /// hide the still-live process.
    ///
    /// Returns `None` when nothing is running or a stop is already in
    /// flight.
    pub fn begin_stop(&mut self) -> Option<(u32, SharedChild)> {
        let running = self.running.as_mut()?;
        if running.stopping {
            return None;
        }
        running.stopping = true;
        Some((running.pid, running.child.clone()))
    }

    /// Reverts `begin_stop` after a failed termination. The process is
    /// still alive, so the entry reports `Running` again instead of
    /// `stopping`, and the profile stays locked.
    pub fn cancel_stop(&mut self, pid: u32) {
        if let Some(running) = self.running.as_mut()
            && running.pid == pid
        {
            running.stopping = false;
        }
    }
}

/// Watches the server process and updates the runtime when it exits.
///
/// Runs as a tokio task instead of a std thread. The child lock is only
/// held for the non-blocking `try_wait` call, so `kill` can still proceed.
pub fn watch(app: AppHandle, child: SharedChild, pid: u32) {
    tauri::async_runtime::spawn(async move {
        let result = loop {
            let status = {
                let mut child = child.lock().await;
                child.try_wait()
            };

            match status {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => tokio::time::sleep(PROCESS_POLL_INTERVAL).await,
                Err(err) => break Err(err),
            }
        };

        match result {
            Ok(status) => info!(pid, ?status, "dedicated server exited"),
            Err(err) => warn!(pid, ?err, "failed to query dedicated server process"),
        }

        let mut runtime = app.lock_server_runtime();
        if runtime.clear_if_pid(pid) {
            emit_status(&app, &runtime.status());
        }
    });
}

/// Kills the process identified by `pid`/`child` and reports the resulting
/// status.
///
/// The runtime entry must have been marked via [`ServerRuntime::begin_stop`].
/// It stays registered and keeps the profile locked for the whole kill.
/// On success the entry is cleared; on failure [`cancel_stop`] returns it
/// to the running state, because reporting a live process as stopped would
/// silently unlock its profile.
pub async fn kill(app: AppHandle, pid: u32, child: SharedChild) -> Result<()> {
    let result = {
        let mut child = child.lock().await;
        child.kill().await
    };

    let mut runtime = app.lock_server_runtime();
    match result {
        Ok(()) => {
            runtime.clear_if_pid(pid);
            emit_status(&app, &runtime.status());
            Ok(())
        }
        Err(err) => {
            warn!(?err, "failed to kill dedicated server process");
            runtime.cancel_stop(pid);
            emit_status(&app, &runtime.status());
            Err(eyre::eyre!("failed to stop the dedicated server: {err}"))
        }
    }
}

pub fn emit_status(app: &AppHandle, status: &ServerStatus) {
    if let Err(err) = app.emit("server_status_changed", status) {
        warn!(?err, "failed to emit dedicated server status");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::ServerStatus;

    #[test]
    fn serializes_frontend_field_names() {
        let value = serde_json::to_value(ServerStatus::Running {
            profile_id: 7,
            game_slug: "valheim".to_owned(),
            pid: 123,
            server_dir: PathBuf::from("server"),
            stopping: true,
        })
        .unwrap();

        assert_eq!(value["profileId"], 7);
        assert_eq!(value["gameSlug"], "valheim");
        assert_eq!(value["serverDir"], "server");
        assert_eq!(value["stopping"], true);
    }

    /// A real long-running child process for the runtime state tests.
    /// The tests only need a process that stays alive; what it does is
    /// irrelevant.
    #[cfg(windows)]
    fn sleeper() -> super::SharedChild {
        let child = tokio::process::Command::new("cmd")
            .args(["/c", "ping", "-n", "60", "127.0.0.1"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        std::sync::Arc::new(tokio::sync::Mutex::new(child))
    }

    #[cfg(not(windows))]
    fn sleeper() -> super::SharedChild {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        std::sync::Arc::new(tokio::sync::Mutex::new(child))
    }

    fn runtime_with_child() -> (super::ServerRuntime, u32) {
        let child = sleeper();
        let pid = child.blocking_lock().id().unwrap();
        let mut runtime = super::ServerRuntime::default();
        runtime
            .register(
                7,
                crate::game::list().next().unwrap(),
                PathBuf::from("server"),
                pid,
                child,
            )
            .unwrap();
        (runtime, pid)
    }

    #[test]
    fn stopping_keeps_the_profile_locked_and_blocks_launch() {
        // The 8.2 contract: while termination is pending the process may
        // still be alive, so it keeps counting as running.
        let (mut runtime, pid) = runtime_with_child();
        assert!(runtime.is_profile_locked(7));

        let (stopped_pid, _child) = runtime.begin_stop().unwrap();
        assert_eq!(stopped_pid, pid);
        assert!(runtime.is_running(), "a stopping server is still running");
        assert!(runtime.is_profile_locked(7));
        assert!(matches!(
            runtime.status(),
            super::ServerStatus::Running { stopping: true, .. }
        ));

        // A second stop attempt does not produce a second kill on a
        // possibly-different process view.
        assert!(runtime.begin_stop().is_none());
        // Registering a replacement is refused while the original lives.
        assert!(
            runtime
                .register(
                    9,
                    crate::game::list().next().unwrap(),
                    PathBuf::from("x"),
                    1,
                    sleeper()
                )
                .is_err()
        );
    }

    #[test]
    fn failed_termination_restores_the_running_entry() {
        // cancel_stop simulates a failed kill: the process is still alive,
        // so the runtime goes back to plain Running. It must never report
        // a live process as stopped.
        let (mut runtime, pid) = runtime_with_child();
        runtime.begin_stop().unwrap();

        runtime.cancel_stop(pid);

        assert!(matches!(
            runtime.status(),
            super::ServerStatus::Running {
                stopping: false,
                ..
            }
        ));
        assert!(runtime.is_profile_locked(7));
        // A retry can stop it again.
        assert!(runtime.begin_stop().is_some());
        // Termination is confirmed and the lock releases.
        assert!(runtime.clear_if_pid(pid));
        assert!(!runtime.is_running());
        assert!(!runtime.is_profile_locked(7));
    }

    #[test]
    fn stale_watcher_cannot_clear_a_replacement() {
        // clear_if_pid is pid-scoped: an old watcher reporting death must
        // not remove a newer registration.
        let (mut runtime, pid) = runtime_with_child();
        runtime.begin_stop().unwrap();
        runtime.clear_if_pid(pid);

        let child = sleeper();
        let new_pid = child.blocking_lock().id().unwrap();
        runtime
            .register(
                7,
                crate::game::list().next().unwrap(),
                PathBuf::from("server"),
                new_pid,
                child,
            )
            .unwrap();

        assert!(!runtime.clear_if_pid(pid), "stale pid cleared a new server");
        assert!(runtime.is_running());
        assert!(runtime.is_profile_locked(7));
    }
}
