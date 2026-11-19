//! Every state transition: the `Lifecycle` implementation and the locked
//! primitives it shares (boot, stop, sleep, delete, provision).
//!
//! Public trait methods that change state run their body on a detached task
//! (see [`Manager::detached`]) that owns the per-instance lock, so a client
//! disconnecting mid-request cannot strand an instance in a transitional state.
//! Primitives named `*_locked` expect the caller to hold that lock.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::OwnedMutexGuard;
use warren_core::{
    Backup, CreateInstanceRequest, DEFAULT_EXEC_TIMEOUT_SEC, DEFAULT_IDLE_TIMEOUT_SEC, Error,
    ExecRequest, ExecResult, ForkRequest, Instance, InstanceStatus, InstanceType, Lifecycle,
    LogsOptions, MAX_BACKUP_BYTES_PER_WORKSPACE, MAX_BACKUPS_PER_INSTANCE, MetricPoint,
    NetworkMode, PatchInstanceRequest, Result, Runtime, STOP_GRACE_SECS, ShapeRequest, Template,
    Workspace,
    errors::{CODE_HOST_CAPACITY_REACHED, is_requires_restart},
    ids, shapes,
};

use crate::{
    manager::{Manager, conflict_for},
    validate::{
        resolve_region, validate_env, validate_idle_timeout, validate_metadata, validate_name,
        validate_note,
    },
};

/// Template used when a create request names none.
pub const DEFAULT_TEMPLATE: &str = "warren-base";
/// How often `ensure_awake` re-reads a transitional instance.
pub const WAKE_POLL: Duration = Duration::from_millis(250);
/// How long `ensure_awake` waits for a transitional instance to run.
pub const WAKE_DEADLINE: Duration = Duration::from_secs(90);
/// Longest metrics window, in hours (one week).
pub const MAX_METRIC_HOURS: u32 = 168;
/// Refusal when a workspace's backups would exceed [`MAX_BACKUP_BYTES_PER_WORKSPACE`].
pub const BACKUP_QUOTA_MESSAGE: &str = "backup storage quota reached";

fn grace() -> Duration {
    Duration::from_secs(STOP_GRACE_SECS)
}

/// Remove a backup: an archive file for container and microVM instances, a
/// directory of copy-on-write clones for mac instances.
async fn remove_backup_path(path: &std::path::Path) -> std::io::Result<()> {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(e) => Err(e),
    }
}

fn transitional(status: InstanceStatus) -> bool {
    matches!(
        status,
        InstanceStatus::Provisioning
            | InstanceStatus::Starting
            | InstanceStatus::Waking
            | InstanceStatus::Restarting
            | InstanceStatus::Updating
    )
}

/// States in which the disk may be read, written or the runtime rebuilt.
fn settled(status: InstanceStatus) -> bool {
    matches!(
        status,
        InstanceStatus::Running
            | InstanceStatus::Stopped
            | InstanceStatus::Sleeping
            | InstanceStatus::Failed
    )
}

fn bytes_of(b: &Backup) -> u64 {
    b.size_bytes.max(0) as u64
}

impl Manager {
    /// Start the runtime and move the instance through `via` into `Running`.
    /// A start failure leaves the instance `Failed` with the error as reason.
    /// For an instance that already holds an awake slot (restart, update,
    /// restore, the fork source); new admissions go through [`Self::boot_admitted`].
    pub(crate) async fn boot(&self, inst: &mut Instance, via: InstanceStatus) -> Result<()> {
        let rt = self.runtime_for(inst)?;
        let from = inst.status;
        self.transition(inst, via, "").await?;
        self.start_runtime(rt, inst, from).await
    }

    /// Admit the instance to the workspace's awake set and boot it. The quota
    /// check and the transition into `via` (which is what the awake count
    /// sees) happen under the workspace lock, so concurrent
    /// create/start/wake/fork requests cannot together exceed the quota.
    pub(crate) async fn boot_admitted(
        &self,
        inst: &mut Instance,
        via: InstanceStatus,
    ) -> Result<()> {
        let rt = self.runtime_for(inst)?;
        let from = inst.status;
        {
            let _ws = self.ws_lock(&inst.workspace_id).await;
            self.check_awake_quota(&inst.workspace_id).await?;
            self.transition(inst, via, "").await?;
        }
        self.start_runtime(rt, inst, from).await
    }

    /// Start the runtime for an instance already moved into a transitional
    /// status; `from` is where it was before, which a host-capacity refusal
    /// returns it to.
    async fn start_runtime(
        &self,
        rt: Arc<dyn Runtime>,
        inst: &mut Instance,
        from: InstanceStatus,
    ) -> Result<()> {
        match rt.start(inst).await {
            Ok(info) => {
                if !info.ip.is_empty() {
                    inst.ip = info.ip;
                }
                if !info.runtime_ref.is_empty() {
                    inst.runtime_ref = info.runtime_ref;
                }
                let now = self.now();
                inst.started_at = now;
                inst.last_activity_at = now;
                self.touch_activity(&inst.id, now);
                self.transition(inst, InstanceStatus::Running, "").await
            }
            Err(e) if e.code == CODE_HOST_CAPACITY_REACHED => {
                // Nothing is wrong with the instance: the host is full. Put the
                // row back where it was so the next attempt can simply retry.
                let back = if from == InstanceStatus::Sleeping {
                    InstanceStatus::Sleeping
                } else {
                    InstanceStatus::Stopped
                };
                inst.ip.clear();
                if back == InstanceStatus::Stopped && inst.stopped_at == 0 {
                    inst.stopped_at = self.now();
                }
                if let Err(save) = self.transition(inst, back, &e.message).await {
                    tracing::error!(instance = %inst.id, error = %save, "could not record the capacity refusal");
                }
                Err(e)
            }
            Err(e) => {
                self.fail(inst, &e, "start").await;
                Err(e)
            }
        }
    }

    /// Stop the runtime and mark the instance `Stopped` with `reason`. A sleeping
    /// instance's runtime is already down, so its stop is best effort and skips
    /// the `Stopping` state.
    pub(crate) async fn stop_locked(&self, inst: &mut Instance, reason: &str) -> Result<()> {
        let rt = self.runtime_for(inst)?;
        if inst.status == InstanceStatus::Sleeping {
            if let Err(e) = rt.stop(inst, grace()).await {
                tracing::debug!(instance = %inst.id, error = %e, "stop of a sleeping runtime reported an error");
            }
        } else {
            self.transition(inst, InstanceStatus::Stopping, reason)
                .await?;
            if let Err(e) = rt.stop(inst, grace()).await
                && !e.is_not_found()
            {
                self.fail_force(inst, &e, "stop").await;
                return Err(e);
            }
        }
        inst.stopped_at = self.now();
        inst.ip.clear();
        self.transition(inst, InstanceStatus::Stopped, reason).await
    }

    /// Put the runtime to sleep (stopped, or suspended with its memory saved
    /// where the runtime can), keep the disk and mark the instance `Sleeping`.
    /// The status is written before the stop so a request arriving meanwhile
    /// queues on the lock and wakes the instance instead of reaching a dying
    /// runtime.
    pub(crate) async fn sleep_locked(&self, inst: &mut Instance, reason: &str) -> Result<()> {
        let rt = self.runtime_for(inst)?;
        inst.slept_at = self.now();
        self.transition(inst, InstanceStatus::Sleeping, reason)
            .await?;
        if let Err(e) = rt.sleep(inst, grace()).await
            && !e.is_not_found()
        {
            self.fail_force(inst, &e, "sleep").await;
            return Err(e);
        }
        inst.ip.clear();
        self.save(inst).await
    }

    /// Wake a sleeping instance once the workspace quota allows one more awake
    /// instance. The `instance_limit` refusal passes through unchanged.
    pub(crate) async fn wake_locked(&self, inst: &mut Instance) -> Result<()> {
        self.boot_admitted(inst, InstanceStatus::Waking).await
    }

    /// Destroy the runtime object and its disk, then remove every record.
    pub(crate) async fn delete_locked(&self, inst: &mut Instance) -> Result<()> {
        self.transition(inst, InstanceStatus::Deleting, "").await?;
        match self.runtime_for(inst) {
            Ok(rt) => match rt.destroy(inst, true).await {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {
                    tracing::debug!(instance = %inst.id, "runtime object already gone");
                }
                Err(e) => {
                    self.fail_force(inst, &e, "destroy").await;
                    return Err(e);
                }
            },
            Err(e) => tracing::error!(
                instance = %inst.id,
                storage = %inst.storage_path,
                error = %e,
                "no runtime can destroy this instance; its storage is left behind"
            ),
        }
        self.purge_records(inst).await
    }

    /// Remove backups (rows and files), crons and the instance row.
    pub(crate) async fn purge_records(&self, inst: &Instance) -> Result<()> {
        let id = inst.id.as_str();
        if let Err(e) = self.deps.store.delete_backups_for_instance(id).await {
            tracing::warn!(instance = id, error = %e, "could not delete backup rows");
        }
        let dir = self.deps.config.backups_dir().join(id);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(instance = id, dir = %dir.display(), error = %e, "could not remove backup files")
            }
        }
        match self.deps.store.list_crons(id).await {
            Ok(crons) => {
                for c in crons {
                    if let Err(e) = self.deps.store.delete_cron(id, &c.id).await {
                        tracing::warn!(instance = id, cron = %c.id, error = %e, "could not delete cron");
                    }
                }
            }
            Err(e) => tracing::warn!(instance = id, error = %e, "could not list crons"),
        }
        self.deps.store.delete_instance(id).await?;
        self.forget(id);
        tracing::info!(instance = id, "deleted");
        Ok(())
    }

    /// Change the network policy. Runtimes rebuild their object for that, so a
    /// running instance is stopped, recreated and started again.
    async fn apply_network(&self, inst: &mut Instance, network: NetworkMode) -> Result<()> {
        let rt = self.runtime_for(inst)?;
        let tpl = self.effective_template(inst).await?;
        let was_running = inst.status == InstanceStatus::Running;
        if was_running {
            self.transition(inst, InstanceStatus::Updating, "").await?;
            if let Err(e) = rt.stop(inst, grace()).await
                && !e.is_not_found()
            {
                self.fail_force(inst, &e, "stop").await;
                return Err(e);
            }
        }
        let previous = inst.network;
        inst.network = network;
        if let Err(e) = rt.recreate(inst, &tpl).await {
            inst.network = previous;
            self.fail(inst, &e, "recreate").await;
            return Err(e);
        }
        if was_running {
            self.boot(inst, InstanceStatus::Updating).await
        } else {
            self.save(inst).await
        }
    }

    /// The provisioning task spawned by `create`: pull, create, start. Holds the
    /// instance lock it was handed for the whole duration.
    async fn provision(
        self: Arc<Self>,
        mut inst: Instance,
        tpl: Template,
        rt: Arc<dyn Runtime>,
        _guard: OwnedMutexGuard<()>,
    ) {
        let result: Result<()> = async {
            rt.ensure_image(&tpl.image_ref).await?;
            rt.create(&mut inst, &tpl).await?;
            self.save(&mut inst).await?;
            self.boot(&mut inst, InstanceStatus::Provisioning).await
        }
        .await;
        match result {
            Ok(()) => tracing::info!(instance = %inst.id, ip = %inst.ip, "provisioned"),
            Err(e) => {
                // Already failed, or parked `stopped` by a host-capacity refusal.
                if !settled(inst.status) {
                    self.fail(&mut inst, &e, "provision").await;
                }
            }
        }
    }

    fn fill_exec_defaults(req: &ExecRequest) -> ExecRequest {
        let mut req = req.clone();
        if req.timeout_seconds == 0 {
            req.timeout_seconds = DEFAULT_EXEC_TIMEOUT_SEC;
        }
        req
    }

    /// Where a backup's archive lives: the stored path, or the conventional
    /// one for rows written without it.
    fn backup_path(&self, b: &Backup) -> PathBuf {
        if b.path.is_empty() {
            self.deps
                .config
                .backups_dir()
                .join(&b.instance_id)
                .join(format!("{}.tar.gz", b.id))
        } else {
            PathBuf::from(&b.path)
        }
    }

    /// Bytes held by every backup of every instance in the workspace, summed
    /// from the per-instance lists (the store has no workspace-level query).
    pub(crate) async fn workspace_backup_bytes(&self, workspace_id: &str) -> Result<u64> {
        let mut total = 0u64;
        for inst in self.deps.store.list_instances(workspace_id).await? {
            for b in self.deps.store.list_backups(&inst.id).await? {
                total = total.saturating_add(bytes_of(&b));
            }
        }
        Ok(total)
    }

    // ------------------------------------------------------------ locked bodies

    async fn start_locked(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        match inst.status {
            InstanceStatus::Running => {}
            InstanceStatus::Stopped | InstanceStatus::Sleeping | InstanceStatus::Failed => {
                self.boot_admitted(&mut inst, InstanceStatus::Starting)
                    .await?;
            }
            _ => return Err(conflict_for(&inst)),
        }
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn stop_op_locked(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        match inst.status {
            InstanceStatus::Stopped => {}
            InstanceStatus::Running | InstanceStatus::Sleeping => {
                self.stop_locked(&mut inst, "").await?;
            }
            _ => return Err(conflict_for(&inst)),
        }
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn sleep_op_locked(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        match inst.status {
            InstanceStatus::Sleeping => {}
            InstanceStatus::Running => self.sleep_locked(&mut inst, "").await?,
            _ => return Err(conflict_for(&inst)),
        }
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn restart_locked(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        if inst.status != InstanceStatus::Running {
            return Err(conflict_for(&inst));
        }
        let rt = self.runtime_for(&inst)?;
        self.transition(&mut inst, InstanceStatus::Restarting, "")
            .await?;
        if let Err(e) = rt.stop(&inst, grace()).await
            && !e.is_not_found()
        {
            self.fail_force(&mut inst, &e, "stop").await;
            return Err(e);
        }
        self.boot(&mut inst, InstanceStatus::Restarting).await?;
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn resize_locked(
        &self,
        workspace_id: &str,
        id: &str,
        req: &ShapeRequest,
    ) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        if !settled(inst.status) {
            return Err(conflict_for(&inst));
        }
        let rt = self.runtime_for(&inst)?;
        // Normalize against the current shape: omitted fields keep their values.
        let base = Template {
            default_shape: inst.shape,
            ..Default::default()
        };
        let mut shape = shapes::normalize(Some(req), Some(&base), inst.kind)?;
        if req.disk.is_none() && shape.disk < inst.shape.disk {
            shape.disk = inst.shape.disk;
            let max = shapes::max_disk(shape, inst.kind);
            if shape.disk > max {
                return Err(Error::invalid(format!(
                    "the current {} GB disk exceeds the {max} GB maximum of the {} vCPU / {} GB shape",
                    shape.disk, shape.cpu, shape.memory
                )));
            }
        }
        if shape.disk < inst.shape.disk {
            return Err(Error::invalid(format!(
                "disk may not shrink below the current {} GB",
                inst.shape.disk
            )));
        }
        if shape == inst.shape {
            self.overlay(&mut inst);
            return Ok(inst);
        }
        match rt.resize(&mut inst, shape).await {
            Ok(()) => {}
            Err(e) if is_requires_restart(&e) => {
                if inst.status == InstanceStatus::Running {
                    self.transition(&mut inst, InstanceStatus::Restarting, "")
                        .await?;
                    if let Err(e) = rt.stop(&inst, grace()).await
                        && !e.is_not_found()
                    {
                        self.fail_force(&mut inst, &e, "stop").await;
                        return Err(e);
                    }
                    if let Err(e) = rt.resize(&mut inst, shape).await {
                        self.fail(&mut inst, &e, "resize").await;
                        return Err(e);
                    }
                    inst.shape = shape;
                    self.boot(&mut inst, InstanceStatus::Restarting).await?;
                    self.overlay(&mut inst);
                    return Ok(inst);
                }
                rt.resize(&mut inst, shape).await?;
            }
            Err(e) => return Err(e),
        }
        inst.shape = shape;
        self.save(&mut inst).await?;
        tracing::info!(instance = %inst.id, shape = %shape, "resized");
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn update_locked(
        &self,
        workspace_id: &str,
        id: &str,
        template: Option<&str>,
    ) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        if !settled(inst.status) {
            return Err(conflict_for(&inst));
        }
        // No pin follows the latest revision of the current template.
        let target = template
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| inst.template_name.clone());
        let tpl = self.deps.templates.resolve(workspace_id, &target).await?;
        if !tpl.allows(inst.kind) {
            return Err(Error::invalid(format!(
                "template {} does not allow type {}",
                tpl.name, inst.kind
            )));
        }
        let rt = self.runtime_for(&inst)?;
        rt.ensure_image(&tpl.image_ref).await?;
        let previous = inst.status;
        let was_running = previous == InstanceStatus::Running;
        self.transition(&mut inst, InstanceStatus::Updating, "")
            .await?;
        if was_running
            && let Err(e) = rt.stop(&inst, grace()).await
            && !e.is_not_found()
        {
            self.fail_force(&mut inst, &e, "stop").await;
            return Err(e);
        }
        if let Err(e) = rt.recreate(&mut inst, &tpl).await {
            self.fail(&mut inst, &e, "recreate").await;
            return Err(e);
        }
        inst.template = target;
        inst.template_name = tpl.name.clone();
        inst.template_revision = tpl.revision;
        inst.image_ref = tpl.image_ref.clone();
        inst.default_port = tpl.default_port;
        inst.mount_path = tpl.mount_path.clone();
        inst.agent_api = tpl.agent;
        if was_running {
            self.boot(&mut inst, InstanceStatus::Updating).await?;
        } else {
            let back = if previous == InstanceStatus::Failed {
                InstanceStatus::Stopped
            } else {
                previous
            };
            self.transition(&mut inst, back, "").await?;
        }
        tracing::info!(instance = %inst.id, template = %inst.template_name, revision = inst.template_revision, image = %inst.image_ref, "updated");
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn fork_locked(&self, ws: &Workspace, id: &str, req: &ForkRequest) -> Result<Instance> {
        let mut src = self.load(&ws.id, id).await?;
        if !matches!(
            src.status,
            InstanceStatus::Running | InstanceStatus::Stopped | InstanceStatus::Sleeping
        ) {
            return Err(conflict_for(&src));
        }
        let rt = self.runtime_for(&src)?;
        let tpl = self.effective_template(&src).await?;
        let base = Template {
            default_shape: src.shape,
            ..Default::default()
        };
        let shape = shapes::normalize(req.shape.as_ref(), Some(&base), src.kind)?;

        // The workspace lock spans the quota check and the insert of the
        // child, which is what consumes the awake slot. The child needs a
        // slot of its own on the host (the source takes its slot back after
        // the copy), so a full host is refused before the source is touched.
        let ws_guard = self.ws_lock(&ws.id).await;
        self.check_awake_quota(&ws.id).await?;
        rt.check_capacity().await?;

        let was_running = src.status == InstanceStatus::Running;
        if was_running {
            self.sleep_locked(&mut src, "fork").await?;
        }
        let now = self.now();
        let dst_id = ids::new_instance_id();
        let mut dst = Instance {
            url: self.deps.config.instance_url(&dst_id),
            id: dst_id,
            workspace_id: ws.id.clone(),
            name: req.name.clone().unwrap_or_else(|| src.name.clone()),
            template: src.template.clone(),
            template_name: src.template_name.clone(),
            template_revision: src.template_revision,
            image_ref: src.image_ref.clone(),
            kind: src.kind,
            status: InstanceStatus::Provisioning,
            status_reason: String::new(),
            shape,
            region: src.region.clone(),
            metadata: req.metadata.clone().unwrap_or_else(|| src.metadata.clone()),
            env: src.env.clone(),
            network: src.network,
            auto_sleep: req.auto_sleep.unwrap_or(src.auto_sleep),
            idle_timeout_seconds: req.idle_timeout_seconds.unwrap_or(src.idle_timeout_seconds),
            default_port: src.default_port,
            mount_path: src.mount_path.clone(),
            agent_api: src.agent_api,
            parent_id: src.id.clone(),
            created_at: now,
            updated_at: now,
            runtime: src.runtime.clone(),
            instance_token: ids::new_instance_token(),
            ..Default::default()
        };
        let dst_guard = self.lock(&dst.id).await;
        self.deps.store.create_instance(&dst).await?;
        drop(ws_guard);
        tracing::info!(instance = %dst.id, parent = %src.id, "forking");
        let result: Result<()> = async {
            rt.clone_storage(&src, &mut dst).await?;
            rt.create(&mut dst, &tpl).await?;
            self.save(&mut dst).await?;
            self.boot(&mut dst, InstanceStatus::Provisioning).await
        }
        .await;
        if let Err(e) = result
            && !settled(dst.status)
        {
            self.fail(&mut dst, &e, "fork").await;
        }
        drop(dst_guard);
        if was_running && let Err(e) = self.boot(&mut src, InstanceStatus::Waking).await {
            tracing::error!(instance = %src.id, error = %e, "could not wake the fork source");
        }
        self.overlay(&mut dst);
        Ok(dst)
    }

    async fn restore_locked(
        &self,
        workspace_id: &str,
        id: &str,
        backup_id: &str,
    ) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        let backup = self.deps.store.get_backup(&inst.id, backup_id).await?;
        if !settled(inst.status) {
            return Err(conflict_for(&inst));
        }
        let rt = self.runtime_for(&inst)?;
        let path = self.backup_path(&backup);
        if tokio::fs::metadata(&path).await.is_err() {
            return Err(Error::not_found("backup file"));
        }
        let was_running = inst.status == InstanceStatus::Running;
        if was_running {
            self.stop_locked(&mut inst, "restore").await?;
        }
        if let Err(e) = rt.restore_storage(&inst, &path).await {
            self.fail(&mut inst, &e, "restore").await;
            return Err(e);
        }
        tracing::info!(instance = %inst.id, backup = %backup.id, "restored");
        if was_running {
            self.boot(&mut inst, InstanceStatus::Starting).await?;
        }
        self.overlay(&mut inst);
        Ok(inst)
    }

    /// The wake step of `ensure_awake`: under the lock, wake the instance if it
    /// is still sleeping and return the row as it is afterwards.
    async fn wake_if_sleeping_locked(&self, id: &str) -> Result<Instance> {
        let mut inst = self.deps.store.get_instance(id).await?;
        if inst.status == InstanceStatus::Sleeping {
            tracing::info!(instance = id, "waking on request");
            self.wake_locked(&mut inst).await?;
        }
        Ok(inst)
    }
}

#[async_trait]
impl Lifecycle for Manager {
    async fn create(&self, ws: &Workspace, req: &CreateInstanceRequest) -> Result<Instance> {
        let name = req.name.clone().unwrap_or_default();
        validate_name(&name)?;
        let metadata = req.metadata.clone().unwrap_or_default();
        validate_metadata(&metadata)?;
        let env = req.env.clone().unwrap_or_default();
        validate_env(&env)?;
        let idle = req.idle_timeout_seconds.unwrap_or(DEFAULT_IDLE_TIMEOUT_SEC);
        validate_idle_timeout(idle, self.deps.config.min_idle_timeout_sec)?;
        let region = resolve_region(req.region.as_deref(), &self.deps.config.region)?;
        if req.kind == Some(InstanceType::Performance) {
            return Err(Error::not_supported(
                "performance instances are not available in this build",
            ));
        }
        let template_ref = req
            .template
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(DEFAULT_TEMPLATE)
            .to_string();
        let tpl = self.deps.templates.resolve(&ws.id, &template_ref).await?;
        // No type in the request: the template decides (`mac` for warren-macos).
        let kind = req.kind.unwrap_or_else(|| tpl.default_type());
        if !tpl.allows(kind) {
            return Err(Error::invalid(format!(
                "template {} does not allow type {kind}",
                tpl.name
            )));
        }
        let rt = self.runtime_for_kind(kind)?;
        let shape = shapes::normalize(req.shape.as_ref(), Some(&tpl), kind)?;

        let now = self.now();
        let id = ids::new_instance_id();
        let inst = Instance {
            url: self.deps.config.instance_url(&id),
            id,
            workspace_id: ws.id.clone(),
            name,
            template: template_ref,
            template_name: tpl.name.clone(),
            template_revision: tpl.revision,
            image_ref: tpl.image_ref.clone(),
            kind,
            status: InstanceStatus::Provisioning,
            status_reason: String::new(),
            shape,
            region,
            metadata,
            env,
            network: req.network.unwrap_or_default(),
            auto_sleep: req.auto_sleep.unwrap_or(false),
            idle_timeout_seconds: idle,
            default_port: tpl.default_port,
            mount_path: tpl.mount_path.clone(),
            agent_api: tpl.agent,
            parent_id: String::new(),
            created_at: now,
            updated_at: now,
            runtime: rt.name().to_string(),
            instance_token: ids::new_instance_token(),
            ..Default::default()
        };
        // The workspace lock spans the quota check and the insert that
        // consumes the awake slot; the instance lock is taken before the row
        // exists so nothing interleaves with provisioning. A full host (a Mac
        // with two macOS guests running) is refused here too, before a VM is
        // cloned for nothing.
        let ws_guard = self.ws_lock(&ws.id).await;
        self.check_awake_quota(&ws.id).await?;
        rt.check_capacity().await?;
        let guard = self.lock(&inst.id).await;
        self.deps.store.create_instance(&inst).await?;
        drop(ws_guard);
        tracing::info!(
            instance = %inst.id,
            workspace = %ws.id,
            template = %inst.template_name,
            kind = %kind,
            shape = %shape,
            "creating"
        );
        let me = self.arc()?;
        tokio::spawn(me.provision(inst.clone(), tpl, rt, guard));
        Ok(inst)
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let mut inst = self.load(workspace_id, id).await?;
        self.overlay(&mut inst);
        Ok(inst)
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<Instance>> {
        let mut list = self.deps.store.list_instances(workspace_id).await?;
        for inst in &mut list {
            self.overlay(inst);
        }
        list.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(list)
    }

    async fn patch(
        &self,
        workspace_id: &str,
        id: &str,
        req: &PatchInstanceRequest,
    ) -> Result<Instance> {
        let (ws, id, req) = (workspace_id.to_string(), id.to_string(), req.clone());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            let mut inst = me.load(&ws, &id).await?;
            if let Some(name) = &req.name {
                validate_name(name)?;
                inst.name = name.clone();
            }
            if let Some(metadata) = &req.metadata {
                validate_metadata(metadata)?;
                inst.metadata = metadata.clone();
            }
            if let Some(env) = &req.env {
                validate_env(env)?;
                inst.env = env.clone();
            }
            if let Some(auto_sleep) = req.auto_sleep {
                inst.auto_sleep = auto_sleep;
            }
            if let Some(idle) = req.idle_timeout_seconds {
                validate_idle_timeout(idle, me.deps.config.min_idle_timeout_sec)?;
                inst.idle_timeout_seconds = idle;
            }
            if let Some(network) = req.network.filter(|n| *n != inst.network) {
                if !settled(inst.status) {
                    return Err(conflict_for(&inst));
                }
                me.apply_network(&mut inst, network).await?;
            }
            me.save(&mut inst).await?;
            me.overlay(&mut inst);
            Ok(inst)
        })
        .await
    }

    async fn start(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.start_locked(&ws, &id).await
        })
        .await
    }

    async fn stop(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.stop_op_locked(&ws, &id).await
        })
        .await
    }

    async fn sleep(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.sleep_op_locked(&ws, &id).await
        })
        .await
    }

    async fn restart(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.restart_locked(&ws, &id).await
        })
        .await
    }

    async fn delete(&self, workspace_id: &str, id: &str) -> Result<()> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            let mut inst = me.load(&ws, &id).await?;
            me.delete_locked(&mut inst).await
        })
        .await
    }

    async fn resize(&self, workspace_id: &str, id: &str, req: &ShapeRequest) -> Result<Instance> {
        let (ws, id, req) = (workspace_id.to_string(), id.to_string(), *req);
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.resize_locked(&ws, &id, &req).await
        })
        .await
    }

    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        template: Option<&str>,
    ) -> Result<Instance> {
        let (ws, id) = (workspace_id.to_string(), id.to_string());
        let template = template.map(str::to_string);
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.update_locked(&ws, &id, template.as_deref()).await
        })
        .await
    }

    async fn fork(&self, ws: &Workspace, id: &str, req: &ForkRequest) -> Result<Instance> {
        if let Some(n) = &req.name {
            validate_name(n)?;
        }
        if let Some(m) = &req.metadata {
            validate_metadata(m)?;
        }
        if let Some(idle) = req.idle_timeout_seconds {
            validate_idle_timeout(idle, self.deps.config.min_idle_timeout_sec)?;
        }
        let (ws, id, req) = (ws.clone(), id.to_string(), req.clone());
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.fork_locked(&ws, &id, &req).await
        })
        .await
    }

    async fn exec(&self, workspace_id: &str, id: &str, req: &ExecRequest) -> Result<ExecResult> {
        if req.command.trim().is_empty() {
            return Err(Error::invalid("command is required"));
        }
        let mut inst = self.load(workspace_id, id).await?;
        if inst.status != InstanceStatus::Running {
            if inst.status == InstanceStatus::Sleeping || transitional(inst.status) {
                inst = self.ensure_awake(id).await?;
            } else {
                return Err(conflict_for(&inst));
            }
        }
        let rt = self.runtime_for(&inst)?;
        self.record_activity(id);
        let out = rt.exec(&inst, &Self::fill_exec_defaults(req)).await;
        self.record_activity(id);
        out
    }

    async fn logs(&self, workspace_id: &str, id: &str, opts: LogsOptions) -> Result<String> {
        let inst = self.load(workspace_id, id).await?;
        if inst.runtime_ref.is_empty() {
            return Ok(String::new());
        }
        self.runtime_for(&inst)?.logs(&inst, opts).await
    }

    async fn metrics(&self, workspace_id: &str, id: &str, hours: u32) -> Result<Vec<MetricPoint>> {
        let inst = self.load(workspace_id, id).await?;
        let hours = hours.clamp(1, MAX_METRIC_HOURS);
        let since = self.now() - i64::from(hours) * 3600;
        let mut points = self.deps.store.list_metrics(&inst.id, since).await?;
        points.sort_by_key(|p| std::cmp::Reverse(p.at));
        Ok(points)
    }

    /// Take a backup, keeping at most [`MAX_BACKUPS_PER_INSTANCE`] per instance
    /// (the oldest are dropped, file and row) and refusing once the workspace's
    /// backups would exceed [`MAX_BACKUP_BYTES_PER_WORKSPACE`].
    async fn backup(&self, workspace_id: &str, id: &str, note: &str) -> Result<Backup> {
        validate_note(note)?;
        let _g = self.lock(id).await;
        let inst = self.load(workspace_id, id).await?;
        if !settled(inst.status) {
            return Err(conflict_for(&inst));
        }
        if inst.runtime_ref.is_empty() && inst.storage_path.is_empty() {
            return Err(Error::conflict("instance has no disk to back up yet"));
        }
        let rt = self.runtime_for(&inst)?;

        let mut existing = self.deps.store.list_backups(&inst.id).await?;
        existing.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let excess = (existing.len() + 1).saturating_sub(MAX_BACKUPS_PER_INSTANCE);
        let (pruned, kept) = existing.split_at(excess.min(existing.len()));
        let pruned_bytes = pruned.iter().map(bytes_of).fold(0u64, u64::saturating_add);
        let ws_bytes = self
            .workspace_backup_bytes(workspace_id)
            .await?
            .saturating_sub(pruned_bytes);
        if ws_bytes >= MAX_BACKUP_BYTES_PER_WORKSPACE {
            return Err(Error::conflict(BACKUP_QUOTA_MESSAGE));
        }

        let backup_id = ids::new_id();
        let dir = self.deps.config.backups_dir().join(&inst.id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{backup_id}.tar.gz"));
        let size = rt.backup_storage(&inst, &path).await?;
        if ws_bytes.saturating_add(size.max(0) as u64) > MAX_BACKUP_BYTES_PER_WORKSPACE {
            if let Err(e) = remove_backup_path(&path).await {
                tracing::warn!(instance = %inst.id, path = %path.display(), error = %e, "could not remove an over-quota backup");
            }
            return Err(Error::conflict(BACKUP_QUOTA_MESSAGE));
        }
        let backup = Backup {
            id: backup_id,
            instance_id: inst.id.clone(),
            workspace_id: inst.workspace_id.clone(),
            path: path.to_string_lossy().into_owned(),
            size_bytes: size,
            created_at: self.now(),
            note: note.to_string(),
        };
        if pruned.is_empty() {
            self.deps.store.create_backup(&backup).await?;
        } else {
            // The store deletes backup rows per instance only, so the kept rows
            // are written back after the whole set is cleared.
            self.deps
                .store
                .delete_backups_for_instance(&inst.id)
                .await?;
            for b in kept.iter().chain(std::iter::once(&backup)) {
                self.deps.store.create_backup(b).await?;
            }
            for b in pruned {
                let old = self.backup_path(b);
                match remove_backup_path(&old).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(instance = %inst.id, backup = %b.id, error = %e, "could not remove an old backup file")
                    }
                }
                tracing::info!(instance = %inst.id, backup = %b.id, "dropped the oldest backup");
            }
        }
        tracing::info!(instance = %inst.id, backup = %backup.id, bytes = size, "backup taken");
        Ok(backup)
    }

    async fn list_backups(&self, workspace_id: &str, id: &str) -> Result<Vec<Backup>> {
        let inst = self.load(workspace_id, id).await?;
        let mut list = self.deps.store.list_backups(&inst.id).await?;
        list.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(list)
    }

    async fn restore(&self, workspace_id: &str, id: &str, backup_id: &str) -> Result<Instance> {
        let (ws, id, backup_id) = (
            workspace_id.to_string(),
            id.to_string(),
            backup_id.to_string(),
        );
        let key = id.clone();
        self.detached(&key, move |me, _g| async move {
            me.restore_locked(&ws, &id, &backup_id).await
        })
        .await
    }

    async fn ensure_awake(&self, id: &str) -> Result<Instance> {
        let wait = self.wake_deadline();
        let deadline = tokio::time::Instant::now() + wait;
        let mut inst = self.deps.store.get_instance(id).await?;
        loop {
            match inst.status {
                InstanceStatus::Running => {
                    self.overlay(&mut inst);
                    return Ok(inst);
                }
                InstanceStatus::Sleeping => {
                    // The wake runs detached so a client that gives up waiting
                    // cannot leave the instance `Waking` forever. A quota
                    // refusal passes through unchanged.
                    let key = id.to_string();
                    inst = self
                        .detached(id, move |me, _g| async move {
                            me.wake_if_sleeping_locked(&key).await
                        })
                        .await?;
                    // Running now, or changed under us: the loop re-evaluates.
                }
                s if transitional(s) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::timeout(format!(
                            "instance is still {s} after {:.1} seconds",
                            wait.as_secs_f64()
                        )));
                    }
                    tokio::time::sleep(WAKE_POLL).await;
                    inst = self.deps.store.get_instance(id).await?;
                }
                _ => return Err(conflict_for(&inst)),
            }
        }
    }

    fn record_activity(&self, id: &str) {
        self.touch_activity(id, self.now());
    }

    async fn endpoint(&self, id: &str) -> Result<String> {
        self.gateway_credentials(id).await.map(|(ip, _)| ip)
    }

    async fn gateway_credentials(&self, id: &str) -> Result<(String, String)> {
        let inst = self.deps.store.get_instance(id).await?;
        if inst.status != InstanceStatus::Running {
            return Err(conflict_for(&inst));
        }
        if inst.ip.is_empty() {
            return Err(Error::bad_gateway("instance has no address"));
        }
        Ok((inst.ip, inst.instance_token))
    }
}
