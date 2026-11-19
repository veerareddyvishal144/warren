//! In-memory fakes for the traits the lifecycle consumes: `Store`, `Runtime`,
//! `TemplateService` and `Clock`. Test-only.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use parking_lot::Mutex;
use warren_core::{
    ApiKey, Backup, Clock, Cron, Error, ExecRequest, ExecResult, Instance, LogsOptions,
    MetricPoint, RegisterTemplateRequest, Result, RunInfo, Runtime, Shape, Stats, Store, Template,
    TemplateService, Workspace, errors,
};

// ---------------------------------------------------------------- clock

/// A settable clock.
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn new(now: i64) -> Arc<Self> {
        Arc::new(FakeClock(AtomicI64::new(now)))
    }
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------- store

#[derive(Default)]
struct State {
    workspaces: BTreeMap<String, Workspace>,
    instances: BTreeMap<String, Instance>,
    templates: Vec<Template>,
    keys: Vec<ApiKey>,
    metrics: Vec<(String, MetricPoint)>,
    backups: Vec<Backup>,
    crons: Vec<Cron>,
}

/// In-memory `Store`.
#[derive(Default)]
pub struct FakeStore {
    state: Mutex<State>,
    /// Number of `update_instance` calls.
    pub updates: AtomicUsize,
    /// Delay inside `count_awake_instances`, in milliseconds, to widen the
    /// window between a quota check and the write that consumes the slot.
    pub count_delay_ms: AtomicU64,
    /// Make `update_instance` fail.
    pub fail_update: AtomicBool,
}

impl FakeStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn put_workspace(&self, ws: Workspace) {
        self.state.lock().workspaces.insert(ws.id.clone(), ws);
    }
    pub fn put_instance(&self, inst: Instance) {
        self.state.lock().instances.insert(inst.id.clone(), inst);
    }
    pub fn instance(&self, id: &str) -> Instance {
        self.state
            .lock()
            .instances
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no instance {id} in the fake store"))
    }
    pub fn has_instance(&self, id: &str) -> bool {
        self.state.lock().instances.contains_key(id)
    }
    pub fn metrics_for(&self, id: &str) -> Vec<MetricPoint> {
        self.state
            .lock()
            .metrics
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, p)| *p)
            .collect()
    }
    pub fn put_metric(&self, id: &str, p: MetricPoint) {
        self.state.lock().metrics.push((id.to_string(), p));
    }
    pub fn backups_for(&self, id: &str) -> Vec<Backup> {
        self.state
            .lock()
            .backups
            .iter()
            .filter(|b| b.instance_id == id)
            .cloned()
            .collect()
    }
    pub fn put_backup(&self, b: Backup) {
        self.state.lock().backups.push(b);
    }
    pub fn crons_for(&self, id: &str) -> Vec<Cron> {
        self.state
            .lock()
            .crons
            .iter()
            .filter(|c| c.instance_id == id)
            .cloned()
            .collect()
    }
    pub fn put_cron(&self, c: Cron) {
        self.state.lock().crons.push(c);
    }
}

#[async_trait]
impl Store for FakeStore {
    async fn create_workspace(&self, ws: &Workspace) -> Result<()> {
        self.put_workspace(ws.clone());
        Ok(())
    }
    async fn get_workspace(&self, id: &str) -> Result<Workspace> {
        self.state
            .lock()
            .workspaces
            .get(id)
            .cloned()
            .ok_or_else(|| Error::not_found("workspace"))
    }
    async fn update_workspace(&self, ws: &Workspace) -> Result<()> {
        let mut st = self.state.lock();
        if !st.workspaces.contains_key(&ws.id) {
            return Err(Error::not_found("workspace"));
        }
        st.workspaces.insert(ws.id.clone(), ws.clone());
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        Ok(self.state.lock().workspaces.values().cloned().collect())
    }

    async fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        self.state.lock().keys.push(k.clone());
        Ok(())
    }
    async fn get_api_key_by_hash(&self, hash: &str) -> Result<ApiKey> {
        self.state
            .lock()
            .keys
            .iter()
            .find(|k| k.hash == hash)
            .cloned()
            .ok_or_else(|| Error::not_found("api key"))
    }
    async fn list_api_keys(&self, workspace_id: &str) -> Result<Vec<ApiKey>> {
        Ok(self
            .state
            .lock()
            .keys
            .iter()
            .filter(|k| k.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
    async fn delete_api_key(&self, workspace_id: &str, id: &str) -> Result<()> {
        let mut st = self.state.lock();
        let before = st.keys.len();
        st.keys
            .retain(|k| !(k.workspace_id == workspace_id && k.id == id));
        if st.keys.len() == before {
            Err(Error::not_found("api key"))
        } else {
            Ok(())
        }
    }
    async fn touch_api_key(&self, id: &str, at: i64) -> Result<()> {
        if let Some(k) = self.state.lock().keys.iter_mut().find(|k| k.id == id) {
            k.last_used_at = at;
        }
        Ok(())
    }

    async fn upsert_template(&self, t: &Template) -> Result<()> {
        let mut st = self.state.lock();
        st.templates.retain(|x| {
            !(x.name == t.name && x.workspace_id == t.workspace_id && x.revision == t.revision)
        });
        st.templates.push(t.clone());
        Ok(())
    }
    async fn get_template(&self, workspace_id: &str, name: &str) -> Result<Template> {
        self.state
            .lock()
            .templates
            .iter()
            .filter(|t| {
                t.name == name && (t.workspace_id.is_empty() || t.workspace_id == workspace_id)
            })
            .max_by_key(|t| t.revision)
            .cloned()
            .ok_or_else(|| Error::not_found("template"))
    }
    async fn get_template_revision(
        &self,
        workspace_id: &str,
        name: &str,
        revision: u32,
    ) -> Result<Template> {
        self.state
            .lock()
            .templates
            .iter()
            .find(|t| {
                t.name == name
                    && t.revision == revision
                    && (t.workspace_id.is_empty() || t.workspace_id == workspace_id)
            })
            .cloned()
            .ok_or_else(|| Error::not_found("template"))
    }
    async fn list_templates(&self, workspace_id: &str) -> Result<Vec<Template>> {
        Ok(self
            .state
            .lock()
            .templates
            .iter()
            .filter(|t| t.workspace_id.is_empty() || t.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
    async fn delete_template(&self, workspace_id: &str, name: &str) -> Result<()> {
        self.state
            .lock()
            .templates
            .retain(|t| !(t.name == name && t.workspace_id == workspace_id));
        Ok(())
    }

    async fn create_instance(&self, inst: &Instance) -> Result<()> {
        let mut st = self.state.lock();
        if st.instances.contains_key(&inst.id) {
            return Err(Error::conflict("instance exists"));
        }
        st.instances.insert(inst.id.clone(), inst.clone());
        Ok(())
    }
    async fn get_instance(&self, id: &str) -> Result<Instance> {
        self.state
            .lock()
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| Error::not_found("instance"))
    }
    async fn get_instance_for_workspace(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        self.state
            .lock()
            .instances
            .get(id)
            .filter(|i| i.workspace_id == workspace_id)
            .cloned()
            .ok_or_else(|| Error::not_found("instance"))
    }
    async fn list_instances(&self, workspace_id: &str) -> Result<Vec<Instance>> {
        Ok(self
            .state
            .lock()
            .instances
            .values()
            .filter(|i| i.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
    async fn list_all_instances(&self) -> Result<Vec<Instance>> {
        Ok(self.state.lock().instances.values().cloned().collect())
    }
    async fn update_instance(&self, inst: &Instance) -> Result<()> {
        if self.fail_update.load(Ordering::SeqCst) {
            return Err(Error::internal("update failed"));
        }
        self.updates.fetch_add(1, Ordering::SeqCst);
        let mut st = self.state.lock();
        if !st.instances.contains_key(&inst.id) {
            return Err(Error::not_found("instance"));
        }
        st.instances.insert(inst.id.clone(), inst.clone());
        Ok(())
    }
    async fn delete_instance(&self, id: &str) -> Result<()> {
        self.state
            .lock()
            .instances
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| Error::not_found("instance"))
    }
    async fn count_awake_instances(&self, workspace_id: &str) -> Result<u32> {
        let awake = self
            .state
            .lock()
            .instances
            .values()
            .filter(|i| i.workspace_id == workspace_id && i.status.awake())
            .count() as u32;
        let delay = self.count_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        Ok(awake)
    }
    async fn count_instances_using_template(&self, workspace_id: &str, name: &str) -> Result<u32> {
        Ok(self
            .state
            .lock()
            .instances
            .values()
            .filter(|i| i.workspace_id == workspace_id && i.template_name == name)
            .count() as u32)
    }

    async fn insert_metric(&self, instance_id: &str, p: MetricPoint) -> Result<()> {
        self.put_metric(instance_id, p);
        Ok(())
    }
    async fn list_metrics(&self, instance_id: &str, since: i64) -> Result<Vec<MetricPoint>> {
        Ok(self
            .state
            .lock()
            .metrics
            .iter()
            .filter(|(i, p)| i == instance_id && p.at >= since)
            .map(|(_, p)| *p)
            .collect())
    }
    async fn prune_metrics(&self, before: i64) -> Result<()> {
        self.state.lock().metrics.retain(|(_, p)| p.at >= before);
        Ok(())
    }

    async fn create_backup(&self, b: &Backup) -> Result<()> {
        self.put_backup(b.clone());
        Ok(())
    }
    async fn list_backups(&self, instance_id: &str) -> Result<Vec<Backup>> {
        Ok(self.backups_for(instance_id))
    }
    async fn get_backup(&self, instance_id: &str, id: &str) -> Result<Backup> {
        self.state
            .lock()
            .backups
            .iter()
            .find(|b| b.instance_id == instance_id && b.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found("backup"))
    }
    async fn delete_backups_for_instance(&self, instance_id: &str) -> Result<()> {
        self.state
            .lock()
            .backups
            .retain(|b| b.instance_id != instance_id);
        Ok(())
    }

    async fn create_cron(&self, c: &Cron) -> Result<()> {
        self.put_cron(c.clone());
        Ok(())
    }
    async fn get_cron(&self, instance_id: &str, id: &str) -> Result<Cron> {
        self.state
            .lock()
            .crons
            .iter()
            .find(|c| c.instance_id == instance_id && c.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found("cron"))
    }
    async fn list_crons(&self, instance_id: &str) -> Result<Vec<Cron>> {
        Ok(self.crons_for(instance_id))
    }
    async fn list_due_crons(&self, now: i64) -> Result<Vec<Cron>> {
        Ok(self
            .state
            .lock()
            .crons
            .iter()
            .filter(|c| c.enabled && c.next_run_at <= now)
            .cloned()
            .collect())
    }
    async fn update_cron(&self, c: &Cron) -> Result<()> {
        let mut st = self.state.lock();
        let slot = st
            .crons
            .iter_mut()
            .find(|x| x.instance_id == c.instance_id && x.id == c.id)
            .ok_or_else(|| Error::not_found("cron"))?;
        *slot = c.clone();
        Ok(())
    }
    async fn delete_cron(&self, instance_id: &str, id: &str) -> Result<()> {
        let mut st = self.state.lock();
        let before = st.crons.len();
        st.crons
            .retain(|c| !(c.instance_id == instance_id && c.id == id));
        if st.crons.len() == before {
            Err(Error::not_found("cron"))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- runtime

/// A `Runtime` that keeps a set of running ids and records every call as
/// `"<op>:<instance id>"`. Failures and behaviours are toggled with atomics.
#[derive(Default)]
pub struct FakeRuntime {
    pub calls: Mutex<Vec<String>>,
    running: Mutex<HashSet<String>>,
    pub fail_ensure_image: AtomicBool,
    pub fail_create: AtomicBool,
    pub fail_start: AtomicBool,
    /// Make `start` answer `host_capacity_reached` (the host is full).
    pub refuse_start_capacity: AtomicBool,
    pub fail_stop: AtomicBool,
    pub fail_destroy: AtomicBool,
    pub fail_recreate: AtomicBool,
    pub fail_restore: AtomicBool,
    /// `resize` answers `requires_restart` while the instance runs.
    pub resize_requires_restart: AtomicBool,
    pub resize_calls: AtomicUsize,
    /// Delay inside `start`, in milliseconds.
    pub start_delay_ms: AtomicU64,
    /// Delay inside `stop`, in milliseconds.
    pub stop_delay_ms: AtomicU64,
    /// `is_running` answers `Err` (the daemon cannot be reached).
    pub liveness_unknown: AtomicBool,
    pub disk_usage: AtomicI64,
    pub starts: AtomicUsize,
    pub recreates: AtomicUsize,
}

impl FakeRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn record(&self, op: &str, id: &str) {
        self.calls.lock().push(format!("{op}:{id}"));
    }
    pub fn is_running(&self, id: &str) -> bool {
        self.running.lock().contains(id)
    }
    pub fn set_running(&self, id: &str, running: bool) {
        let mut r = self.running.lock();
        if running {
            r.insert(id.to_string());
        } else {
            r.remove(id);
        }
    }
    /// Operations recorded for `id`, in order.
    pub fn ops_for(&self, id: &str) -> Vec<String> {
        let suffix = format!(":{id}");
        self.calls
            .lock()
            .iter()
            .filter_map(|c| c.strip_suffix(&suffix).map(str::to_string))
            .collect()
    }
    fn failing(flag: &AtomicBool, what: &str) -> Result<()> {
        if flag.load(Ordering::SeqCst) {
            Err(Error::internal(format!("{what} failed")))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl Runtime for FakeRuntime {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn ready(&self) -> Result<()> {
        Ok(())
    }
    async fn ensure_image(&self, image_ref: &str) -> Result<String> {
        self.record("ensure_image", image_ref);
        Self::failing(&self.fail_ensure_image, "image pull")?;
        Ok(format!("sha256:{image_ref}"))
    }
    async fn create(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        self.record("create", &inst.id);
        Self::failing(&self.fail_create, "create")?;
        if inst.storage_path.is_empty() {
            inst.storage_path = format!("/fake/instances/{}/home", inst.id);
        }
        inst.runtime_ref = format!("ctr-{}-{}", inst.id, tpl.image_ref);
        Ok(())
    }
    async fn start(&self, inst: &mut Instance) -> Result<RunInfo> {
        self.record("start", &inst.id);
        let delay = self.start_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        Self::failing(&self.fail_start, "start")?;
        if self.refuse_start_capacity.load(Ordering::SeqCst) {
            return Err(Error::host_capacity(2, 2));
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.set_running(&inst.id, true);
        Ok(RunInfo {
            ip: format!("10.0.0.{}", self.starts.load(Ordering::SeqCst)),
            runtime_ref: inst.runtime_ref.clone(),
        })
    }
    async fn stop(&self, inst: &Instance, _grace: Duration) -> Result<()> {
        self.record("stop", &inst.id);
        let delay = self.stop_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        Self::failing(&self.fail_stop, "stop")?;
        self.set_running(&inst.id, false);
        Ok(())
    }
    async fn is_running(&self, inst: &Instance) -> Result<bool> {
        self.record("is_running", &inst.id);
        if self.liveness_unknown.load(Ordering::SeqCst) {
            return Err(Error::internal("daemon unreachable"));
        }
        Ok(FakeRuntime::is_running(self, &inst.id))
    }
    async fn destroy(&self, inst: &Instance, remove_storage: bool) -> Result<()> {
        self.record(
            if remove_storage {
                "destroy+storage"
            } else {
                "destroy"
            },
            &inst.id,
        );
        Self::failing(&self.fail_destroy, "destroy")?;
        self.set_running(&inst.id, false);
        Ok(())
    }
    async fn exec(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult> {
        self.record("exec", &inst.id);
        if !self.is_running(&inst.id) {
            return Err(Error::conflict("runtime not running"));
        }
        Ok(ExecResult {
            exit_code: 0,
            stdout: format!("ran: {}", req.command),
            ..Default::default()
        })
    }
    async fn logs(&self, inst: &Instance, _opts: LogsOptions) -> Result<String> {
        self.record("logs", &inst.id);
        Ok(format!("logs of {}", inst.id))
    }
    async fn stats(&self, inst: &Instance) -> Result<Stats> {
        if !self.is_running(&inst.id) {
            return Err(Error::not_found("container"));
        }
        Ok(Stats {
            at: 0,
            cpu_percent: 12.5,
            memory_bytes: 1024,
            memory_limit_bytes: 4096,
            disk_bytes: 2048,
        })
    }
    async fn resize(&self, inst: &mut Instance, shape: Shape) -> Result<()> {
        self.record("resize", &inst.id);
        self.resize_calls.fetch_add(1, Ordering::SeqCst);
        if self.resize_requires_restart.load(Ordering::SeqCst) && self.is_running(&inst.id) {
            return Err(errors::requires_restart());
        }
        inst.shape = shape;
        Ok(())
    }
    async fn recreate(&self, inst: &mut Instance, tpl: &Template) -> Result<()> {
        self.record("recreate", &inst.id);
        Self::failing(&self.fail_recreate, "recreate")?;
        let n = self.recreates.fetch_add(1, Ordering::SeqCst) + 1;
        inst.runtime_ref = format!("ctr-{}-{}-v{n}", inst.id, tpl.image_ref);
        Ok(())
    }
    async fn clone_storage(&self, src: &Instance, dst: &mut Instance) -> Result<()> {
        self.record("clone_storage", &dst.id);
        dst.storage_path = format!("{}-clone-of-{}", src.storage_path, src.id);
        Ok(())
    }
    async fn backup_storage(&self, inst: &Instance, dest_path: &Path) -> Result<i64> {
        self.record("backup_storage", &inst.id);
        let body = format!("backup of {}", inst.id);
        std::fs::write(dest_path, &body)?;
        Ok(body.len() as i64)
    }
    async fn restore_storage(&self, inst: &Instance, src_path: &Path) -> Result<()> {
        self.record("restore_storage", &inst.id);
        Self::failing(&self.fail_restore, "restore")?;
        std::fs::metadata(src_path)?;
        Ok(())
    }
    async fn disk_usage(&self, inst: &Instance) -> Result<i64> {
        self.record("disk_usage", &inst.id);
        Ok(self.disk_usage.load(Ordering::SeqCst))
    }
}

// ---------------------------------------------------------------- templates

/// A `TemplateService` over a fixed map; understands `name@revision` pins.
#[derive(Default)]
pub struct FakeTemplates {
    templates: Mutex<Vec<Template>>,
}

impl FakeTemplates {
    pub fn new(templates: Vec<Template>) -> Arc<Self> {
        Arc::new(FakeTemplates {
            templates: Mutex::new(templates),
        })
    }
    pub fn put(&self, t: Template) {
        self.templates.lock().push(t);
    }
    /// Latest revision of `name`; panics when unknown (test convenience).
    pub fn resolve_sync(&self, name: &str) -> Template {
        self.templates
            .lock()
            .iter()
            .filter(|t| t.name == name)
            .max_by_key(|t| t.revision)
            .cloned()
            .unwrap_or_else(|| panic!("no template {name}"))
    }
}

#[async_trait]
impl TemplateService for FakeTemplates {
    async fn resolve(&self, _workspace_id: &str, name: &str) -> Result<Template> {
        let (name, pin) = match name.split_once('@') {
            Some((n, p)) => (
                n,
                Some(p.parse::<u32>().map_err(|_| Error::invalid("bad pin"))?),
            ),
            None => (name, None),
        };
        let all = self.templates.lock();
        let found = match pin {
            Some(rev) => all.iter().find(|t| t.name == name && t.revision == rev),
            None => all
                .iter()
                .filter(|t| t.name == name)
                .max_by_key(|t| t.revision),
        };
        found.cloned().ok_or_else(|| Error::not_found("template"))
    }
    async fn get(&self, workspace_id: &str, name: &str) -> Result<Template> {
        self.resolve(workspace_id, name).await
    }
    async fn list(&self, _workspace_id: &str) -> Result<Vec<Template>> {
        Ok(self.templates.lock().clone())
    }
    async fn register(
        &self,
        _workspace_id: &str,
        _req: &RegisterTemplateRequest,
    ) -> Result<Template> {
        Err(Error::not_supported("not used in lifecycle tests"))
    }
    async fn delete(&self, _workspace_id: &str, _name: &str) -> Result<()> {
        Err(Error::not_supported("not used in lifecycle tests"))
    }
    async fn ensure_system_templates(&self) -> Result<()> {
        Ok(())
    }
}
