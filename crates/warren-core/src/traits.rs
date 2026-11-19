use std::{collections::BTreeMap, collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Result, types::*};

/// Runtimes keyed by the instance type they serve.
pub type Runtimes = Arc<HashMap<InstanceType, Arc<dyn Runtime>>>;

/// Time source; tests substitute a fixed clock.
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
    fn now_unix_ms(&self) -> i64 {
        self.now_unix() * 1000
    }
}

pub struct RealClock;

impl Clock for RealClock {
    fn now_unix(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    fn now_unix_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Persistence. Implementations are `Send + Sync`. Expected failures are
/// `Error::not_found` / `Error::conflict`; infrastructure failures are
/// `Error::internal`.
#[async_trait]
pub trait Store: Send + Sync {
    async fn create_workspace(&self, ws: &Workspace) -> Result<()>;
    async fn get_workspace(&self, id: &str) -> Result<Workspace>;
    async fn update_workspace(&self, ws: &Workspace) -> Result<()>;
    async fn list_workspaces(&self) -> Result<Vec<Workspace>>;

    async fn create_api_key(&self, k: &ApiKey) -> Result<()>;
    async fn get_api_key_by_hash(&self, hash: &str) -> Result<ApiKey>;
    async fn list_api_keys(&self, workspace_id: &str) -> Result<Vec<ApiKey>>;
    async fn delete_api_key(&self, workspace_id: &str, id: &str) -> Result<()>;
    async fn touch_api_key(&self, id: &str, at: i64) -> Result<()>;

    /// System templates have an empty `workspace_id` and are visible to everyone;
    /// workspace templates only to their workspace. Upserting an existing
    /// workspace template increments `revision` and keeps older revisions readable.
    async fn upsert_template(&self, t: &Template) -> Result<()>;
    async fn get_template(&self, workspace_id: &str, name: &str) -> Result<Template>;
    async fn get_template_revision(
        &self,
        workspace_id: &str,
        name: &str,
        revision: u32,
    ) -> Result<Template>;
    async fn list_templates(&self, workspace_id: &str) -> Result<Vec<Template>>;
    async fn delete_template(&self, workspace_id: &str, name: &str) -> Result<()>;

    async fn create_instance(&self, inst: &Instance) -> Result<()>;
    async fn get_instance(&self, id: &str) -> Result<Instance>;
    async fn get_instance_for_workspace(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn list_instances(&self, workspace_id: &str) -> Result<Vec<Instance>>;
    async fn list_all_instances(&self) -> Result<Vec<Instance>>;
    async fn update_instance(&self, inst: &Instance) -> Result<()>;
    async fn delete_instance(&self, id: &str) -> Result<()>;
    async fn count_awake_instances(&self, workspace_id: &str) -> Result<u32>;
    async fn count_instances_using_template(&self, workspace_id: &str, name: &str) -> Result<u32>;

    async fn insert_metric(&self, instance_id: &str, p: MetricPoint) -> Result<()>;
    async fn list_metrics(&self, instance_id: &str, since: i64) -> Result<Vec<MetricPoint>>;
    async fn prune_metrics(&self, before: i64) -> Result<()>;

    async fn create_backup(&self, b: &Backup) -> Result<()>;
    async fn list_backups(&self, instance_id: &str) -> Result<Vec<Backup>>;
    async fn get_backup(&self, instance_id: &str, id: &str) -> Result<Backup>;
    async fn delete_backups_for_instance(&self, instance_id: &str) -> Result<()>;

    async fn create_cron(&self, c: &Cron) -> Result<()>;
    async fn get_cron(&self, instance_id: &str, id: &str) -> Result<Cron>;
    async fn list_crons(&self, instance_id: &str) -> Result<Vec<Cron>>;
    async fn list_due_crons(&self, now: i64) -> Result<Vec<Cron>>;
    async fn update_cron(&self, c: &Cron) -> Result<()>;
    async fn delete_cron(&self, instance_id: &str, id: &str) -> Result<()>;
}

/// Creates and drives instances of one type. Runtimes persist nothing themselves:
/// they fill `runtime_ref`, `storage_path`, `ip` on the instance and the caller saves.
#[async_trait]
pub trait Runtime: Send + Sync {
    fn name(&self) -> &'static str;
    /// Socket reachable, kvm present, binaries found. A failing runtime is disabled.
    async fn ready(&self) -> Result<()>;
    /// Make `image_ref` available locally and return its digest.
    async fn ensure_image(&self, image_ref: &str) -> Result<String>;
    /// Prepare persistent storage and the runtime object without starting it.
    /// Must set `inst.storage_path` and `inst.runtime_ref`.
    async fn create(&self, inst: &mut Instance, tpl: &Template) -> Result<()>;
    /// Run the instance; block until the gateway port accepts connections or a
    /// bounded timeout passes.
    async fn start(&self, inst: &mut Instance) -> Result<RunInfo>;
    async fn stop(&self, inst: &Instance, grace: Duration) -> Result<()>;
    /// Put the instance to sleep: bring the runtime down while keeping the
    /// disk, and the memory too when the runtime can (a macOS VM is suspended
    /// and resumes where it was). `start` wakes it. Defaults to `stop`.
    async fn sleep(&self, inst: &Instance, grace: Duration) -> Result<()> {
        self.stop(inst, grace).await
    }
    /// Remove the runtime object and, when `remove_storage`, the disk.
    async fn destroy(&self, inst: &Instance, remove_storage: bool) -> Result<()>;
    async fn exec(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult>;
    async fn logs(&self, inst: &Instance, opts: LogsOptions) -> Result<String>;
    async fn stats(&self, inst: &Instance) -> Result<Stats>;
    /// Whether the runtime object exists and is running right now. `Ok(false)`
    /// when it exited or does not exist; `Err` means the answer is unknown
    /// (daemon unreachable) and callers must not change instance state on it.
    async fn is_running(&self, inst: &Instance) -> Result<bool> {
        self.stats(inst).await.map(|_| true)
    }
    /// Apply a new shape. Returns `errors::requires_restart()` when it cannot be
    /// done live; the caller stops, resizes again and starts.
    async fn resize(&self, inst: &mut Instance, shape: Shape) -> Result<()>;
    /// Replace the runtime object with one built from `tpl`, keeping storage.
    /// The instance is stopped by the caller first.
    async fn recreate(&self, inst: &mut Instance, tpl: &Template) -> Result<()>;
    /// Copy `src`'s disk into a fresh storage path for `dst` and set
    /// `dst.storage_path`. The caller invokes `create` for `dst` afterwards.
    async fn clone_storage(&self, src: &Instance, dst: &mut Instance) -> Result<()>;
    async fn backup_storage(&self, inst: &Instance, dest_path: &std::path::Path) -> Result<i64>;
    async fn restore_storage(&self, inst: &Instance, src_path: &std::path::Path) -> Result<()>;
    async fn disk_usage(&self, inst: &Instance) -> Result<i64>;
}

/// Body of `POST /v1/instances`. Every field is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInstanceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, rename = "resources", skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeRequest>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<InstanceType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sleep: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkMode>,
}

/// Body of `PATCH /v1/instances/{id}`. Absent fields are unchanged; `metadata`
/// and `env` replace the whole map when present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchInstanceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sleep: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkMode>,
}

/// Body of `POST /v1/instances/{id}/fork`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, rename = "resources", skip_serializing_if = "Option::is_none")]
    pub shape: Option<ShapeRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_sleep: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u32>,
}

/// Body of `POST /v1/templates`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterTemplateRequest {
    pub name: String,
    pub image_ref: String,
    #[serde(default)]
    pub default_port: u16,
    #[serde(default)]
    pub mount_path: String,
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Orchestrates instances: validation, placement, quota checks, runtime calls,
/// state transitions and background loops. The only component that calls
/// `Runtime` and the only one that moves `Instance::status`.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    async fn create(&self, ws: &Workspace, req: &CreateInstanceRequest) -> Result<Instance>;
    async fn get(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<Instance>>;
    async fn patch(
        &self,
        workspace_id: &str,
        id: &str,
        req: &PatchInstanceRequest,
    ) -> Result<Instance>;
    async fn start(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn stop(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn sleep(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn restart(&self, workspace_id: &str, id: &str) -> Result<Instance>;
    async fn delete(&self, workspace_id: &str, id: &str) -> Result<()>;
    async fn resize(&self, workspace_id: &str, id: &str, shape: &ShapeRequest) -> Result<Instance>;
    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        template: Option<&str>,
    ) -> Result<Instance>;
    async fn fork(&self, ws: &Workspace, id: &str, req: &ForkRequest) -> Result<Instance>;
    async fn exec(&self, workspace_id: &str, id: &str, req: &ExecRequest) -> Result<ExecResult>;
    async fn logs(&self, workspace_id: &str, id: &str, opts: LogsOptions) -> Result<String>;
    async fn metrics(&self, workspace_id: &str, id: &str, hours: u32) -> Result<Vec<MetricPoint>>;
    async fn backup(&self, workspace_id: &str, id: &str, note: &str) -> Result<Backup>;
    async fn list_backups(&self, workspace_id: &str, id: &str) -> Result<Vec<Backup>>;
    async fn restore(&self, workspace_id: &str, id: &str, backup_id: &str) -> Result<Instance>;

    /// The edge's wake-on-request hook: a sleeping instance is woken and the call
    /// blocks (bounded) until it is running; stopped or failed returns conflict;
    /// unknown id returns not_found.
    async fn ensure_awake(&self, id: &str) -> Result<Instance>;
    /// Reset the idle timer. Called by the edge on traffic and by exec.
    fn record_activity(&self, id: &str);
    /// IP the edge proxies to for a running instance.
    async fn endpoint(&self, id: &str) -> Result<String>;
    /// IP and per-instance gateway token for a running instance, so the edge and
    /// the cron scheduler can authenticate to the gateway. The default returns an
    /// empty token for fakes; the real lifecycle returns `Instance::instance_token`.
    async fn gateway_credentials(&self, id: &str) -> Result<(String, String)> {
        Ok((self.endpoint(id).await?, String::new()))
    }
}

/// Resolves names and pins, lists the catalog, registers workspace images.
#[async_trait]
pub trait TemplateService: Send + Sync {
    /// "name" or "name@revision"; system templates by name (a pin on a system
    /// template must equal its current revision).
    async fn resolve(&self, workspace_id: &str, name: &str) -> Result<Template>;
    async fn get(&self, workspace_id: &str, name: &str) -> Result<Template>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<Template>>;
    async fn register(&self, workspace_id: &str, req: &RegisterTemplateRequest)
    -> Result<Template>;
    async fn delete(&self, workspace_id: &str, name: &str) -> Result<()>;
    /// Seed the built-in catalog at startup.
    async fn ensure_system_templates(&self) -> Result<()>;
}

/// Mints and verifies signed URL tokens.
pub trait Signer: Send + Sync {
    fn sign(&self, instance_id: &str, port: u16, expires_at: i64) -> String;
    /// Returns (instance id, port).
    fn verify(&self, token: &str) -> Result<(String, u16)>;
}

/// Resolves a plaintext API key (or "Bearer <key>") to its workspace.
#[async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, raw_key: &str) -> Result<(Workspace, ApiKey)>;
}
