//! The worker HTTP API contract, shared between `gale-worker` and the
//! Gale desktop's worker client.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::profile::{
    export::{ConfigPath, ModRevision},
    server::{
        lease::LeaseRecord,
        plan::{DeploySelection, DeploymentPlan},
        settings::RestartPolicy,
        state::{
            OperationKind, OperationRecord, OperationSummary, RestartOutcome, ServerDeploymentState,
        },
    },
    sync::ConfigUpdatePolicy,
};

pub const API_BASE: &str = "/v1";

/// `POST /v1/preview` — compute a plan for the given selection against the
/// worker's view of the canonical publication and the remote server.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRequest {
    pub selection: DeploySelection,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResponse {
    pub plan: DeploymentPlan,
    pub busy: Option<LeaseRecord>,
    pub warnings: Vec<String>,
}

/// `POST /v1/deploy` — execute a previously previewed plan. `plan_hash`
/// binds the request to the approved preview exactly like Local mode.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployRequest {
    pub selection: DeploySelection,
    pub plan_hash: String,
    /// Restart behavior for this operation. `None` uses the worker's
    /// configured policy.
    pub restart_policy: Option<RestartPolicy>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployResponse {
    pub plan: DeploymentPlan,
    pub summary: OperationSummary,
    pub warnings: Vec<String>,
    pub failed_config_writes: Vec<ConfigPath>,
    pub restart: RestartOutcome,
    pub state: ServerDeploymentState,
}

/// `GET /v1/status` — journal state plus, when `refresh` is set, a live
/// read of the remote deployment state.
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
    pub last_seen_revision: Option<DateTime<Utc>>,
    /// The operation currently in flight, if any.
    pub busy: Option<BusyOperation>,
    pub last_operation: Option<OperationRecord>,
    /// The last poll/deploy failure the worker recorded.
    pub last_error: Option<String>,
    /// Live remote state, present only for `?refresh=1` requests.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStateSummary {
    pub mods_revision: Option<ModRevision>,
    pub restart_required: bool,
    pub pending_configs: usize,
    pub last_operation: Option<OperationRecord>,
    /// A live lease held by any executor.
    pub lease: Option<LeaseRecord>,
}

/// `POST /v1/policy` — set a persistent per-file config update policy in
/// the remote deployment state.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRequest {
    pub path: ConfigPath,
    pub policy: ConfigUpdatePolicy,
    /// The published hash the policy is pinned at.
    pub pinned_at: Option<crate::profile::export::ContentHash>,
}

/// `POST /v1/config` — update the worker's automation toggles. Manual
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
