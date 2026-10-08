//! The restart policy applied once a deployment's files are in place.

use std::time::Duration;

use tracing::{info, warn};

use crate::profile::server::{
    host::{HostControl, HostStatus},
    progress::ProgressReporter,
    settings::RestartPolicy,
    state::RestartOutcome,
};

const RESTART_VERIFY_ATTEMPTS: usize = 12;
const RESTART_VERIFY_DELAY: Duration = Duration::from_secs(5);

/// The restart outcome, and why it fell short when it did.
#[derive(Debug)]
pub struct RestartReport {
    pub outcome: RestartOutcome,
    /// The reason a restart failed or could not be verified, for the
    /// deployment's warnings.
    pub warning: Option<String>,
}

impl From<RestartOutcome> for RestartReport {
    fn from(outcome: RestartOutcome) -> Self {
        Self {
            outcome,
            warning: None,
        }
    }
}

/// Consults the host provider for the configured restart policy.
///
/// `NotRequired`/`Awaiting*` outcomes are recorded in the deployment state
/// by [`finish`](super::finish); an unknown player count is never treated as empty.
pub(in crate::profile::server) async fn apply_restart_policy_reporting(
    host: &dyn HostControl,
    policy: RestartPolicy,
    requires_restart: bool,
    progress: &mut ProgressReporter,
) -> RestartReport {
    if !requires_restart {
        progress.item("No restart needed");
        return RestartOutcome::NotRequired.into();
    }

    if !host.can_restart() {
        progress.item("Waiting for a manual restart");
        return RestartOutcome::AwaitingManual.into();
    }

    match policy {
        RestartPolicy::Manual => {
            progress.item("Waiting for a manual restart");
            RestartOutcome::AwaitingManual.into()
        }
        RestartPolicy::Immediate => {
            progress.item("Checking server status");
            do_restart(host, host.status().await.ok(), progress).await
        }
        RestartPolicy::WhenEmpty => match empty_server_status(host, progress).await {
            Some(status) => do_restart(host, Some(status), progress).await,
            None => RestartOutcome::AwaitingEmpty.into(),
        },
    }
}

/// Presence must explicitly report zero before a WhenEmpty restart.
pub(super) async fn empty_server_status(
    host: &dyn HostControl,
    progress: &mut ProgressReporter,
) -> Option<HostStatus> {
    match host.status().await {
        Ok(status) if status.players == Some(0) => Some(status),
        _ => {
            progress.item("Waiting until the server is empty");
            None
        }
    }
}

fn observed_restart(saw_stopped: &mut bool, saw_booting: &mut bool, status: &HostStatus) -> bool {
    if status.running == Some(false) {
        *saw_stopped = true;
    }
    if status.booting == Some(true) {
        *saw_booting = true;
    }
    status.running == Some(true)
        && status.booting != Some(true)
        && (*saw_stopped || (*saw_booting && status.booting == Some(false)))
}

pub(super) async fn do_restart(
    host: &dyn HostControl,
    before: Option<HostStatus>,
    progress: &mut ProgressReporter,
) -> RestartReport {
    info!(host = host.name(), "restarting dedicated server");
    progress.item("Requesting server restart");
    if let Err(error) = host.restart().await {
        warn!(
            host = host.name(),
            "server restart request failed: {error:#}"
        );
        let warning = format!("The restart request failed: {error:#}");
        progress.item(warning.clone());
        return RestartReport {
            outcome: RestartOutcome::Failed,
            warning: Some(warning),
        };
    }

    // A running response alone may still describe the old process. Observe
    // either a stop/start or a post-request booting/completed transition.
    let mut saw_stopped = before
        .as_ref()
        .is_some_and(|status| status.running == Some(false));
    let mut saw_booting = false;
    for attempt in 0..RESTART_VERIFY_ATTEMPTS {
        let waiting_for = if before
            .as_ref()
            .is_some_and(|status| status.booting.is_some())
        {
            "server to finish booting"
        } else {
            "server to stop and start"
        };
        progress.item(format!(
            "Waiting for {waiting_for} (check {} of {})",
            attempt + 1,
            RESTART_VERIFY_ATTEMPTS
        ));
        tokio::time::sleep(RESTART_VERIFY_DELAY).await;
        match host.status().await {
            Ok(status) if observed_restart(&mut saw_stopped, &mut saw_booting, &status) => {
                return RestartOutcome::Restarted.into();
            }
            Ok(_) => {}
            Err(error) => {
                return RestartReport {
                    outcome: RestartOutcome::StartupUnverified,
                    warning: Some(format!(
                        "The restart was requested, but its result could not be checked: {error:#}"
                    )),
                };
            }
        }
    }
    RestartOutcome::StartupUnverified.into()
}
