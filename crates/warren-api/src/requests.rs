//! Request bodies that belong to the Sandbox API alone. The bodies shared with
//! other components (`CreateInstanceRequest`, `PatchInstanceRequest`,
//! `ForkRequest`, `RegisterTemplateRequest`, `ExecRequest`, `ShapeRequest`)
//! live in `warren_core`. Unknown fields are rejected everywhere.

use serde::{Deserialize, Serialize};
use warren_core::Money;

/// `POST /v1/signup`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignupRequest {
    /// Workspace name; defaults to "workspace".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `POST /v1/api-keys`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateKeyRequest {
    /// Display name; defaults to "key".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Unix seconds; absent or 0 means the key never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// `POST /v1/billing/top-up`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopUpRequest {
    pub amount_micros: Money,
    /// Only "dev" exists in this build; it is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// `POST /v1/instances/{id}/update`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInstanceRequest {
    /// "name" or "name@revision"; absent means the instance's own template at
    /// its latest revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// `POST /v1/instances/{id}/backups`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBackupRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `POST /v1/instances/{id}/restore`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub backup_id: String,
}

/// `POST /v1/instances/{id}/crons`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCronRequest {
    pub prompt: String,
    /// Five-field cron expression: minute hour day-of-month month day-of-week.
    pub schedule: String,
    /// IANA name; absent or empty means UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Defaults to true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `PATCH /v1/instances/{id}/crons/{cid}`; absent fields are unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchCronRequest {
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
