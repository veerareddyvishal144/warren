//! In-memory fakes of the traits this crate consumes, for unit tests only.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use warren_core::{
    ApiKey, Backup, Clock, Cron, Error, ExecRequest, ExecResult, Instance, InstanceSecret,
    LogsOptions, MetricPoint, Result, RunInfo, Runtime, Shape, Stats, Store, Template, Workspace,
};

/// (workspace_id, name); system templates use an empty workspace id.
type Key = (String, String);

fn key(workspace_id: &str, name: &str) -> Key {
    (workspace_id.to_string(), name.to_string())
}

fn unused<T>() -> Result<T> {
    Err(Error::internal("not used by warren-templates"))
}

/// Template rows keyed by workspace and name, every revision kept. Like the real
/// store it assigns revisions itself: a name that already exists gets
/// `max + 1`, a new one gets 1, whatever the caller passed.
#[derive(Default)]
pub struct FakeStore {
    templates: Mutex<HashMap<Key, Vec<Template>>>,
    in_use: Mutex<HashMap<Key, u32>>,
    /// Every template handed to `upsert_template`, exactly as the service sent it.
    pub upserts: Mutex<Vec<Template>>,
    /// When set, `get_template` fails with this error.
    pub fail_get: Mutex<Option<Error>>,
}

impl FakeStore {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Insert a row verbatim, keeping its revision.
    pub fn seed(&self, t: Template) {
        self.templates
            .lock()
            .unwrap()
            .entry(key(&t.workspace_id, &t.name))
            .or_default()
            .push(t);
    }

    pub fn set_in_use(&self, workspace_id: &str, name: &str, n: u32) {
        self.in_use
            .lock()
            .unwrap()
            .insert(key(workspace_id, name), n);
    }

    pub fn upsert_count(&self) -> usize {
        self.upserts.lock().unwrap().len()
    }

    pub fn last_upsert(&self) -> Template {
        self.upserts
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("an upsert happened")
    }

    /// Revisions stored for one template, ascending.
    pub fn revisions(&self, workspace_id: &str, name: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .templates
            .lock()
            .unwrap()
            .get(&key(workspace_id, name))
            .map(|rows| rows.iter().map(|r| r.revision).collect())
            .unwrap_or_default();
        out.sort_unstable();
        out
    }

    fn latest(map: &HashMap<Key, Vec<Template>>, k: &Key) -> Option<Template> {
        map.get(k)
            .and_then(|rows| rows.iter().max_by_key(|r| r.revision))
            .cloned()
    }
}

#[async_trait]
impl Store for FakeStore {
    async fn create_workspace(&self, _ws: &Workspace) -> Result<()> {
        unused()
    }
    async fn get_workspace(&self, _id: &str) -> Result<Workspace> {
        unused()
    }
    async fn update_workspace(&self, _ws: &Workspace) -> Result<()> {
        unused()
    }
    async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        unused()
    }

    async fn create_api_key(&self, _k: &ApiKey) -> Result<()> {
        unused()
    }
    async fn get_api_key_by_hash(&self, _hash: &str) -> Result<ApiKey> {
        unused()
    }
    async fn list_api_keys(&self, _workspace_id: &str) -> Result<Vec<ApiKey>> {
        unused()
    }
    async fn delete_api_key(&self, _workspace_id: &str, _id: &str) -> Result<()> {
        unused()
    }
    async fn touch_api_key(&self, _id: &str, _at: i64) -> Result<()> {
        unused()
    }

    async fn upsert_template(&self, t: &Template) -> Result<()> {
        self.upserts.lock().unwrap().push(t.clone());
        let mut map = self.templates.lock().unwrap();
        let rows = map.entry(key(&t.workspace_id, &t.name)).or_default();
        let mut stored = t.clone();
        stored.revision = rows.iter().map(|r| r.revision).max().map_or(1, |m| m + 1);
        rows.push(stored);
        Ok(())
    }

    async fn get_template(&self, workspace_id: &str, name: &str) -> Result<Template> {
        if let Some(e) = self.fail_get.lock().unwrap().clone() {
            return Err(e);
        }
        let map = self.templates.lock().unwrap();
        Self::latest(&map, &key(workspace_id, name))
            .or_else(|| Self::latest(&map, &key("", name)))
            .ok_or_else(|| Error::not_found("template"))
    }

    async fn get_template_revision(
        &self,
        workspace_id: &str,
        name: &str,
        revision: u32,
    ) -> Result<Template> {
        self.templates
            .lock()
            .unwrap()
            .get(&key(workspace_id, name))
            .and_then(|rows| rows.iter().find(|r| r.revision == revision))
            .cloned()
            .ok_or_else(|| Error::not_found("template revision"))
    }

    /// Current revision of every system template and of the workspace's own,
    /// deliberately in reverse name order so the service has to sort.
    async fn list_templates(&self, workspace_id: &str) -> Result<Vec<Template>> {
        let map = self.templates.lock().unwrap();
        let mut out: Vec<Template> = map
            .iter()
            .filter(|((ws, _), _)| ws.is_empty() || ws == workspace_id)
            .filter_map(|(k, _)| Self::latest(&map, k))
            .collect();
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    }

    async fn delete_template(&self, workspace_id: &str, name: &str) -> Result<()> {
        self.templates
            .lock()
            .unwrap()
            .remove(&key(workspace_id, name))
            .map(|_| ())
            .ok_or_else(|| Error::not_found("template"))
    }

    async fn create_instance(&self, _inst: &Instance) -> Result<()> {
        unused()
    }
    async fn get_instance(&self, _id: &str) -> Result<Instance> {
        unused()
    }
    async fn get_instance_for_workspace(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unused()
    }
    async fn list_instances(&self, _workspace_id: &str) -> Result<Vec<Instance>> {
        unused()
    }
    async fn list_all_instances(&self) -> Result<Vec<Instance>> {
        unused()
    }
    async fn update_instance(&self, _inst: &Instance) -> Result<()> {
        unused()
    }
    async fn delete_instance(&self, _id: &str) -> Result<()> {
        unused()
    }
    async fn count_awake_instances(&self, _workspace_id: &str) -> Result<u32> {
        unused()
    }
    async fn count_instances_using_template(&self, workspace_id: &str, name: &str) -> Result<u32> {
        Ok(self
            .in_use
            .lock()
            .unwrap()
            .get(&key(workspace_id, name))
            .copied()
            .unwrap_or(0))
    }

    async fn replace_instance_secrets(
        &self,
        _instance_id: &str,
        _secrets: &[InstanceSecret],
    ) -> Result<()> {
        unused()
    }
    async fn list_instance_secrets(&self, _instance_id: &str) -> Result<Vec<InstanceSecret>> {
        unused()
    }

    async fn insert_metric(&self, _instance_id: &str, _p: MetricPoint) -> Result<()> {
        unused()
    }
    async fn list_metrics(&self, _instance_id: &str, _since: i64) -> Result<Vec<MetricPoint>> {
        unused()
    }
    async fn prune_metrics(&self, _before: i64) -> Result<()> {
        unused()
    }

    async fn create_backup(&self, _b: &Backup) -> Result<()> {
        unused()
    }
    async fn list_backups(&self, _instance_id: &str) -> Result<Vec<Backup>> {
        unused()
    }
    async fn get_backup(&self, _instance_id: &str, _id: &str) -> Result<Backup> {
        unused()
    }
    async fn delete_backups_for_instance(&self, _instance_id: &str) -> Result<()> {
        unused()
    }

    async fn create_cron(&self, _c: &Cron) -> Result<()> {
        unused()
    }
    async fn get_cron(&self, _instance_id: &str, _id: &str) -> Result<Cron> {
        unused()
    }
    async fn list_crons(&self, _instance_id: &str) -> Result<Vec<Cron>> {
        unused()
    }
    async fn list_due_crons(&self, _now: i64) -> Result<Vec<Cron>> {
        unused()
    }
    async fn update_cron(&self, _c: &Cron) -> Result<()> {
        unused()
    }
    async fn delete_cron(&self, _instance_id: &str, _id: &str) -> Result<()> {
        unused()
    }
}

/// Records `ensure_image` calls and answers with a deterministic digest, or with
/// the configured error.
#[derive(Default)]
pub struct FakeRuntime {
    pub pulls: Mutex<Vec<String>>,
    pub fail_pull: Mutex<Option<Error>>,
}

impl FakeRuntime {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn pulls(&self) -> Vec<String> {
        self.pulls.lock().unwrap().clone()
    }

    pub fn digest_for(image_ref: &str) -> String {
        let mut h = DefaultHasher::new();
        image_ref.hash(&mut h);
        format!("sha256:{:016x}", h.finish())
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
        self.pulls.lock().unwrap().push(image_ref.to_string());
        if let Some(e) = self.fail_pull.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(Self::digest_for(image_ref))
    }
    async fn create(&self, _inst: &mut Instance, _tpl: &Template) -> Result<()> {
        unused()
    }
    async fn start(&self, _inst: &mut Instance) -> Result<RunInfo> {
        unused()
    }
    async fn stop(&self, _inst: &Instance, _grace: Duration) -> Result<()> {
        unused()
    }
    async fn destroy(&self, _inst: &Instance, _remove_storage: bool) -> Result<()> {
        unused()
    }
    async fn exec(&self, _inst: &Instance, _req: &ExecRequest) -> Result<ExecResult> {
        unused()
    }
    async fn logs(&self, _inst: &Instance, _opts: LogsOptions) -> Result<String> {
        unused()
    }
    async fn stats(&self, _inst: &Instance) -> Result<Stats> {
        unused()
    }
    async fn resize(&self, _inst: &mut Instance, _shape: Shape) -> Result<()> {
        unused()
    }
    async fn recreate(&self, _inst: &mut Instance, _tpl: &Template) -> Result<()> {
        unused()
    }
    async fn clone_storage(&self, _src: &Instance, _dst: &mut Instance) -> Result<()> {
        unused()
    }
    async fn backup_storage(&self, _inst: &Instance, _dest_path: &std::path::Path) -> Result<i64> {
        unused()
    }
    async fn restore_storage(&self, _inst: &Instance, _src_path: &std::path::Path) -> Result<()> {
        unused()
    }
    async fn disk_usage(&self, _inst: &Instance) -> Result<i64> {
        unused()
    }
}

/// A clock that returns whatever the test set.
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn new(now: i64) -> Arc<Self> {
        Arc::new(FixedClock(AtomicI64::new(now)))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::Relaxed);
    }
}

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}
