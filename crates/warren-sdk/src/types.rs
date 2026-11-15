//! Response and request bodies of the two APIs that have no counterpart in
//! `warren_core`. Everything else (instances, templates, keys, ledger entries,
//! backups, crons, exec, the Agent API objects) is decoded straight into the
//! core types.

use serde::{Deserialize, Serialize};
use warren_core::agentapi::ResponseStatus;
use warren_core::{ApiKey, InstanceStatus, LedgerEntry, Money, Rates, Shape, Workspace};

/// `POST /v1/signup`: a new workspace with its first key, shown once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignupResponse {
    pub workspace: Workspace,
    /// The plaintext key; it is never shown again.
    pub api_key: String,
    pub key_id: String,
    pub wallet_micros: Money,
}

/// `GET /v1/workspace`: the workspace plus its awake-instance counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    #[serde(flatten)]
    pub workspace: Workspace,
    #[serde(default)]
    pub awake_limit: u32,
    #[serde(default)]
    pub awake_count: u32,
}

/// `POST /v1/api-keys`: the key record plus its plaintext, shown once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateApiKeyResponse {
    pub key: ApiKey,
    pub api_key: String,
}

/// `DELETE /v1/api-keys/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedKey {
    pub id: String,
    pub deleted: bool,
}

/// `DELETE /v1/templates/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedTemplate {
    pub name: String,
    pub deleted: bool,
}

/// `GET /v1/billing`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BillingResponse {
    pub wallet_micros: Money,
    #[serde(default)]
    pub top_up_total_micros: Money,
    #[serde(default)]
    pub awake_limit: u32,
    #[serde(default)]
    pub awake_count: u32,
    pub rates: Rates,
    #[serde(default)]
    pub free_credit_granted: bool,
}

/// `POST /v1/billing/top-up`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopUpResponse {
    pub workspace: Workspace,
    pub entry: LedgerEntry,
}

/// The `{ "id", "status" }` answer of stop, start, sleep, restart and delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub id: String,
    pub status: InstanceStatus,
}

/// `POST /v1/instances/{id}/resize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeResponse {
    pub id: String,
    pub status: InstanceStatus,
    pub resources: Shape,
}

/// `POST /v1/instances/{id}/update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResponse {
    pub id: String,
    pub status: InstanceStatus,
    #[serde(default)]
    pub image_ref: String,
    #[serde(default)]
    pub template_revision: u32,
}

/// `GET /v1/instances/{id}/signed-url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUrl {
    pub url: String,
    /// Unix seconds.
    pub expires_at: i64,
}

/// `GET /v1/health` on the Sandbox API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub healthy: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub runtimes: Vec<String>,
    /// True when the daemon runs with `--dev` (dev top-ups accepted).
    #[serde(default)]
    pub dev_mode: bool,
    /// Unix seconds.
    #[serde(default)]
    pub time: i64,
}

/// `GET /v1/version` on either API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    pub version: String,
}

/// `GET /v1/instances/{id}/logs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogsResponse {
    #[serde(default)]
    pub logs: String,
}

/// Body of `POST /v1/instances/{id}/crons`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CronRequest {
    pub prompt: String,
    /// Five-field cron expression.
    pub schedule: String,
    /// IANA name; absent means UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absent means enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl CronRequest {
    /// A cron with just the two required fields.
    pub fn new(prompt: impl Into<String>, schedule: impl Into<String>) -> Self {
        CronRequest {
            prompt: prompt.into(),
            schedule: schedule.into(),
            ..Default::default()
        }
    }
}

/// Body of `PATCH /v1/instances/{id}/crons/{cid}`. Absent fields are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CronPatchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `POST /v1/responses/{id}/cancel` on the Agent API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResponse {
    pub id: String,
    pub status: ResponseStatus,
}

/// `PUT /v1/files/content` on the Agent API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub path: String,
    pub size: u64,
}

/// `DELETE /v1/files` on the Agent API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedFile {
    pub path: String,
    pub deleted: bool,
}
