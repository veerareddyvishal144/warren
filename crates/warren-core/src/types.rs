use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Lifecycle state of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    #[default]
    Provisioning,
    Running,
    Stopping,
    Stopped,
    Starting,
    Restarting,
    Updating,
    Sleeping,
    Waking,
    Failed,
    Deleting,
    Deleted,
}

impl InstanceStatus {
    pub const ALL: [InstanceStatus; 12] = [
        Self::Provisioning,
        Self::Running,
        Self::Stopping,
        Self::Stopped,
        Self::Starting,
        Self::Restarting,
        Self::Updating,
        Self::Sleeping,
        Self::Waking,
        Self::Failed,
        Self::Deleting,
        Self::Deleted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Restarting => "restarting",
            Self::Updating => "updating",
            Self::Sleeping => "sleeping",
            Self::Waking => "waking",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Counts against the workspace's awake-instance limit: everything that holds
    /// CPU and memory or is about to.
    pub fn awake(self) -> bool {
        matches!(
            self,
            Self::Provisioning
                | Self::Running
                | Self::Stopping
                | Self::Starting
                | Self::Restarting
                | Self::Updating
                | Self::Waking
        )
    }

    pub fn terminal(self) -> bool {
        self == Self::Deleted
    }
}

impl std::fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Isolation technology of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstanceType {
    /// A container under gVisor (or runc when gVisor is unavailable) on a shared host.
    #[default]
    Default,
    /// A Firecracker microVM with its own kernel.
    Vm,
    /// Reserved for hosts with pinned cores; not available in the single-node build.
    Performance,
}

impl InstanceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Vm => "vm",
            Self::Performance => "performance",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "vm" => Some(Self::Vm),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }
}

impl std::fmt::Display for InstanceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outbound network policy of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum NetworkMode {
    #[default]
    #[serde(rename = "allow-all")]
    AllowAll,
    #[serde(rename = "deny-all")]
    DenyAll,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowAll => "allow-all",
            Self::DenyAll => "deny-all",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow-all" => Some(Self::AllowAll),
            "deny-all" => Some(Self::DenyAll),
            _ => None,
        }
    }
}

/// CPU, memory (GB) and disk (GB) allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Shape {
    pub cpu: u32,
    pub memory: u32,
    pub disk: u32,
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}v/{}g/{}d", self.cpu, self.memory, self.disk)
    }
}

/// Partial shape as accepted in requests (every field optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ShapeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<u32>,
}

/// A tenant: many instances, many API keys. Capacity is bounded by the daemon's
/// per-workspace quota (`Config::max_awake_instances`), never by money.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

/// An API key. Only the SHA-256 of the plaintext is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApiKey {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub prefix: String,
    #[serde(skip)]
    pub hash: String,
    pub created_at: i64,
    /// Unix seconds; 0 means never.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_used_at: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TemplateScope {
    #[default]
    System,
    Workspace,
}

impl TemplateScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Workspace => "workspace",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Self::System),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }
}

/// A named image plus the port its main service listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Template {
    pub name: String,
    pub scope: TemplateScope,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    pub image_ref: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_digest: String,
    /// 0 means the bare instance URL serves nothing.
    pub default_port: u16,
    /// Where the persistent disk mounts, e.g. /home/agent.
    pub mount_path: String,
    /// Serves the Agent API (POST /v1/responses) on `default_port`.
    pub agent: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Allowed instance types; empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<InstanceType>,
    pub default_shape: Shape,
    /// Defaults merged under the instance's env.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub revision: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Template {
    pub fn allows(&self, t: InstanceType) -> bool {
        self.types.is_empty() || self.types.contains(&t)
    }
}

/// A persistent computer for one user or workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Instance {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// As requested, may carry an @pin.
    pub template: String,
    pub template_name: String,
    pub template_revision: u32,
    pub image_ref: String,
    #[serde(rename = "type")]
    pub kind: InstanceType,
    pub status: InstanceStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_reason: String,
    #[serde(rename = "resources")]
    pub shape: Shape,
    pub region: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub network: NetworkMode,
    pub auto_sleep: bool,
    pub idle_timeout_seconds: u32,
    /// Derived by the API layer from the base domain; not stored.
    #[serde(default)]
    pub url: String,
    pub default_port: u16,
    pub mount_path: String,
    pub agent_api: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stopped_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub slept_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_activity_at: i64,
    /// "docker" or "firecracker".
    pub runtime: String,
    /// Container id or VM id.
    #[serde(skip)]
    pub runtime_ref: String,
    #[serde(skip)]
    pub ip: String,
    #[serde(skip)]
    pub storage_path: String,
    /// Secret the guest gateway requires on /v1/exec.
    #[serde(skip)]
    pub instance_token: String,
    pub disk_used_bytes: i64,
}

impl Instance {
    /// Env the runtime injects: template defaults, then instance env, then the
    /// platform variables (which always win).
    pub fn effective_env(&self, tpl_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut env = tpl_env.clone();
        for (k, v) in &self.env {
            env.insert(k.clone(), v.clone());
        }
        env.insert(ENV_INSTANCE_ID.into(), self.id.clone());
        env.insert(ENV_INSTANCE_TOKEN.into(), self.instance_token.clone());
        env.insert(ENV_WORKSPACE_ID.into(), self.workspace_id.clone());
        env.insert(ENV_HOME.into(), self.mount_path.clone());
        env.insert(ENV_GATEWAY_PORT.into(), GATEWAY_PORT.to_string());
        env
    }
}

/// A restore point for an instance's disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Backup {
    pub id: String,
    pub instance_id: String,
    pub workspace_id: String,
    #[serde(skip)]
    pub path: String,
    pub size_bytes: i64,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// A scheduled prompt for an agent instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cron {
    pub id: String,
    pub instance_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub prompt: String,
    /// Five-field cron expression.
    pub schedule: String,
    /// IANA name; empty means UTC.
    #[serde(default)]
    pub timezone: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_run_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_status: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub next_run_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One shell command inside an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    pub command: String,
    /// Empty means the image's default user.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub timeout_seconds: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LogsOptions {
    #[serde(default)]
    pub tail: u32,
    /// Unix seconds.
    #[serde(default)]
    pub since: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Stats {
    pub at: i64,
    pub cpu_percent: f64,
    pub memory_bytes: i64,
    pub memory_limit_bytes: i64,
    pub disk_bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct MetricPoint {
    pub at: i64,
    pub cpu_percent: f64,
    pub memory_bytes: i64,
    pub disk_bytes: i64,
}

/// What a runtime reports after starting an instance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunInfo {
    pub ip: String,
    pub runtime_ref: String,
}

// Well-known ports, headers and environment names shared by daemon, edge and gateway.
pub const GATEWAY_PORT: u16 = 3737;
pub const TERMINAL_PORT: u16 = 7681;
pub const HEADER_API_KEY: &str = "x-warren-key";
pub const HEADER_INSTANCE_TOKEN: &str = "x-warren-instance-token";
pub const QUERY_TOKEN: &str = "warren_token";
pub const COOKIE_NAME: &str = "warren_session";
pub const ENV_INSTANCE_ID: &str = "WARREN_INSTANCE_ID";
pub const ENV_INSTANCE_TOKEN: &str = "WARREN_INSTANCE_TOKEN";
pub const ENV_WORKSPACE_ID: &str = "WARREN_WORKSPACE_ID";
pub const ENV_HOME: &str = "WARREN_HOME";
pub const ENV_GATEWAY_PORT: &str = "WARREN_GATEWAY_PORT";
pub const ENV_AGENT_DRIVER: &str = "WARREN_AGENT_DRIVER";
pub const ENV_AGENT_COMMAND: &str = "WARREN_AGENT_COMMAND";
pub const ENV_MODEL: &str = "WARREN_MODEL";

// Limits that do not depend on configuration.
pub const MIN_IDLE_TIMEOUT_SEC: u32 = 300;
pub const MAX_IDLE_TIMEOUT_SEC: u32 = 86_400;
pub const DEFAULT_IDLE_TIMEOUT_SEC: u32 = 900;
pub const MAX_EXEC_OUTPUT_BYTES: usize = 1 << 20;
pub const DEFAULT_EXEC_TIMEOUT_SEC: u32 = 15 * 60;
pub const STOP_GRACE_SECS: u64 = 10;
pub const INSTANCE_ID_LENGTH: usize = 10;
pub const MAX_ENV_ENTRIES: usize = 64;
pub const MAX_ENV_VALUE_BYTES: usize = 4096;
pub const MAX_METADATA_ENTRIES: usize = 32;
/// Backups kept per instance; the oldest is deleted when a new one exceeds it.
pub const MAX_BACKUPS_PER_INSTANCE: usize = 20;
/// Total backup bytes a workspace may hold.
pub const MAX_BACKUP_BYTES_PER_WORKSPACE: u64 = 20 << 30;
