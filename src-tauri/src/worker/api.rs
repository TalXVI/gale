//! The worker HTTP API contract, shared between `gale-worker` and the
//! Gale desktop's worker client.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::profile::{
    export::{ConfigPath, ModRevision},
    server::{
        lease::LeaseRecord,
        plan::DeploySelection,
        settings::RestartPolicy,
        state::{OperationKind, OperationRecord},
    },
    sync::ConfigUpdatePolicy,
};

pub use crate::profile::server::engine::{
    DeploymentResult as DeployResponse, Preview as PreviewResponse,
};

pub const API_BASE: &str = "/v1";

/// `POST /v1/preview` computes a plan for the given selection against the
/// worker's view of the canonical publication and the remote server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRequest {
    #[serde(default)]
    pub run_id: String,
    pub selection: DeploySelection,
    /// The restart policy the subsequent deploy will use. Bound into the
    /// plan hash so a policy change after preview invalidates the approval.
    /// `None` uses the worker's configured policy.
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
}

/// `POST /v1/deploy` executes a previously previewed plan. `plan_hash`
/// binds the request to the approved preview exactly like Local mode.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployRequest {
    #[serde(default)]
    pub run_id: String,
    pub selection: DeploySelection,
    pub plan_hash: String,
    /// Restart behavior for this operation. `None` uses the worker's
    /// configured policy.
    pub restart_policy: Option<RestartPolicy>,
    /// Take over a *stale foreign* lease, the documented recovery after
    /// the old executor is confirmed stopped. Live leases always win.
    #[serde(default)]
    pub force: bool,
}

/// `GET /v1/status` returns journal state plus, when `refresh` is set, a
/// live read of the remote deployment state.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponse {
    pub worker_id: String,
    /// The sync profile id this worker is bound to. The caller verifies it
    /// matches the profile it intended to manage.
    pub profile_id: String,
    pub auto_sync: bool,
    pub auto_mods: bool,
    pub restart_policy: RestartPolicy,
    /// The newest publication revision the worker has observed.
    /// Observation alone is not deployment.
    pub observed_revision: Option<DateTime<Utc>>,
    /// A publication revision whose mod payload is still owed, if any.
    /// The worker never owes config work: the server is authoritative
    /// for its config files after setup, so a pending revision always
    /// means the mod payload has not been confirmed deployed.
    pub pending_revision: Option<DateTime<Utc>>,
    /// When the pending work becomes eligible for its next attempt.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// The newest publication revision whose mod payload is confirmed
    /// deployed on the server. Config state plays no part: the server
    /// owns its config files, and an explicit config push never
    /// advances this marker.
    pub last_deployed_revision: Option<DateTime<Utc>>,
    /// The operation currently in flight, if any.
    pub busy: Option<BusyOperation>,
    pub last_operation: Option<OperationRecord>,
    /// The last deployment failure the worker recorded.
    pub last_error: Option<String>,
    /// The current publication-poll failure. Absent on older workers.
    #[serde(default)]
    pub poll_error: Option<String>,
    /// Live remote state, present only for `?refresh=true` requests.
    pub server: Option<ServerStateSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusyOperation {
    pub id: String,
    pub kind: OperationKind,
    pub started_at: DateTime<Utc>,
}

/// A projection of the remote deployment state for status displays.
/// Config reconciliation is not part of routine synchronization, so no
/// config-pending count exists here: undecided configs are surfaced by
/// an explicit config preview, never as background work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStateSummary {
    pub mods_revision: Option<ModRevision>,
    pub restart_required: bool,
    pub last_operation: Option<OperationRecord>,
    /// A live lease held by any executor.
    pub lease: Option<LeaseRecord>,
}

/// `POST /v1/policy` sets a persistent per-file config update policy in
/// the remote deployment state. The worker resolves the policy pin from
/// the canonical publication, so clients cannot supply it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRequest {
    pub path: ConfigPath,
    pub policy: ConfigUpdatePolicy,
}

/// `POST /v1/config` updates the worker's automation toggles. Manual
/// Deploy Now requests are unaffected by `auto_sync`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureRequest {
    pub auto_sync: bool,
    pub auto_mods: bool,
    pub restart_policy: RestartPolicy,
}

/// Error responses carry a single human-readable message.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Why a worker process stopped, as recorded in its status file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerRunPhase {
    /// Serving and polling normally.
    Running,
    /// Stopped by request (service stop control or equivalent).
    Stopped,
    /// Stopped because the OS is shutting down.
    Shutdown,
}

/// The worker's last-known run state, written to `statusFile` when the
/// worker runs as a managed background service. The file lets the desktop
/// distinguish a graceful stop from a crash: after a crash the last write
/// still says `running`, which combined with a stopped service means the
/// process died.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRunReport {
    pub worker_id: String,
    pub profile_id: String,
    pub pid: u32,
    pub phase: WorkerRunPhase,
    pub at: DateTime<Utc>,
}
