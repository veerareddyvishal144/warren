//! `warren_core::Store` for [`SqliteStore`]: every method clones its arguments,
//! hops to the blocking pool and runs the entity module's SQL under the lock.

use async_trait::async_trait;
use warren_core::{
    ApiKey, Backup, Cron, Instance, LedgerEntry, MetricPoint, Result, Store, Template, Workspace,
};

use crate::{
    SqliteStore, api_keys, backups, crons, instances, ledger, metrics, templates, workspaces,
};

#[async_trait]
impl Store for SqliteStore {
    async fn create_workspace(&self, ws: &Workspace) -> Result<()> {
        let ws = ws.clone();
        self.run(move |c| workspaces::create(c, &ws)).await
    }

    async fn get_workspace(&self, id: &str) -> Result<Workspace> {
        let id = id.to_owned();
        self.run(move |c| workspaces::get(c, &id)).await
    }

    async fn update_workspace(&self, ws: &Workspace) -> Result<()> {
        let ws = ws.clone();
        self.run(move |c| workspaces::update(c, &ws)).await
    }

    async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        self.run(|c| workspaces::list(c)).await
    }

    async fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        let k = k.clone();
        self.run(move |c| api_keys::create(c, &k)).await
    }

    async fn get_api_key_by_hash(&self, hash: &str) -> Result<ApiKey> {
        let hash = hash.to_owned();
        self.run(move |c| api_keys::get_by_hash(c, &hash)).await
    }

    async fn list_api_keys(&self, workspace_id: &str) -> Result<Vec<ApiKey>> {
        let workspace_id = workspace_id.to_owned();
        self.run(move |c| api_keys::list(c, &workspace_id)).await
    }

    async fn delete_api_key(&self, workspace_id: &str, id: &str) -> Result<()> {
        let (workspace_id, id) = (workspace_id.to_owned(), id.to_owned());
        self.run(move |c| api_keys::delete(c, &workspace_id, &id))
            .await
    }

    async fn touch_api_key(&self, id: &str, at: i64) -> Result<()> {
        let id = id.to_owned();
        self.run(move |c| api_keys::touch(c, &id, at)).await
    }

    async fn upsert_template(&self, t: &Template) -> Result<()> {
        let t = t.clone();
        self.run(move |c| templates::upsert(c, &t)).await
    }

    async fn get_template(&self, workspace_id: &str, name: &str) -> Result<Template> {
        let (workspace_id, name) = (workspace_id.to_owned(), name.to_owned());
        self.run(move |c| templates::get(c, &workspace_id, &name))
            .await
    }

    async fn get_template_revision(
        &self,
        workspace_id: &str,
        name: &str,
        revision: u32,
    ) -> Result<Template> {
        let (workspace_id, name) = (workspace_id.to_owned(), name.to_owned());
        self.run(move |c| templates::get_revision(c, &workspace_id, &name, revision))
            .await
    }

    async fn list_templates(&self, workspace_id: &str) -> Result<Vec<Template>> {
        let workspace_id = workspace_id.to_owned();
        self.run(move |c| templates::list(c, &workspace_id)).await
    }

    async fn delete_template(&self, workspace_id: &str, name: &str) -> Result<()> {
        let (workspace_id, name) = (workspace_id.to_owned(), name.to_owned());
        self.run(move |c| templates::delete(c, &workspace_id, &name))
            .await
    }

    async fn create_instance(&self, inst: &Instance) -> Result<()> {
        let inst = inst.clone();
        self.run(move |c| instances::create(c, &inst)).await
    }

    async fn get_instance(&self, id: &str) -> Result<Instance> {
        let id = id.to_owned();
        self.run(move |c| instances::get(c, &id)).await
    }

    async fn get_instance_for_workspace(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        let (workspace_id, id) = (workspace_id.to_owned(), id.to_owned());
        self.run(move |c| instances::get_for_workspace(c, &workspace_id, &id))
            .await
    }

    async fn list_instances(&self, workspace_id: &str) -> Result<Vec<Instance>> {
        let workspace_id = workspace_id.to_owned();
        self.run(move |c| instances::list(c, &workspace_id)).await
    }

    async fn list_all_instances(&self) -> Result<Vec<Instance>> {
        self.run(|c| instances::list_all(c)).await
    }

    async fn update_instance(&self, inst: &Instance) -> Result<()> {
        let inst = inst.clone();
        self.run(move |c| instances::update(c, &inst)).await
    }

    async fn delete_instance(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.run(move |c| instances::delete(c, &id)).await
    }

    async fn set_meter(
        &self,
        id: &str,
        metered_through: i64,
        carry_micros: f64,
        expected_from: Option<i64>,
    ) -> Result<bool> {
        let id = id.to_owned();
        self.run(move |c| {
            instances::set_meter(c, &id, metered_through, carry_micros, expected_from)
        })
        .await
    }

    async fn count_awake_instances(&self, workspace_id: &str) -> Result<u32> {
        let workspace_id = workspace_id.to_owned();
        self.run(move |c| instances::count_awake(c, &workspace_id))
            .await
    }

    async fn count_instances_using_template(&self, workspace_id: &str, name: &str) -> Result<u32> {
        let (workspace_id, name) = (workspace_id.to_owned(), name.to_owned());
        self.run(move |c| templates::count_instances_using(c, &workspace_id, &name))
            .await
    }

    async fn apply_ledger(&self, entry: &mut LedgerEntry) -> Result<()> {
        let mut e = entry.clone();
        let e = self
            .run(move |c| {
                ledger::apply(c, &mut e)?;
                Ok(e)
            })
            .await?;
        *entry = e;
        Ok(())
    }

    async fn list_ledger(&self, workspace_id: &str, limit: u32) -> Result<Vec<LedgerEntry>> {
        let workspace_id = workspace_id.to_owned();
        self.run(move |c| ledger::list(c, &workspace_id, limit))
            .await
    }

    async fn insert_metric(&self, instance_id: &str, p: MetricPoint) -> Result<()> {
        let instance_id = instance_id.to_owned();
        self.run(move |c| metrics::insert(c, &instance_id, p)).await
    }

    async fn list_metrics(&self, instance_id: &str, since: i64) -> Result<Vec<MetricPoint>> {
        let instance_id = instance_id.to_owned();
        self.run(move |c| metrics::list(c, &instance_id, since))
            .await
    }

    async fn prune_metrics(&self, before: i64) -> Result<()> {
        self.run(move |c| metrics::prune(c, before)).await
    }

    async fn create_backup(&self, b: &Backup) -> Result<()> {
        let b = b.clone();
        self.run(move |c| backups::create(c, &b)).await
    }

    async fn list_backups(&self, instance_id: &str) -> Result<Vec<Backup>> {
        let instance_id = instance_id.to_owned();
        self.run(move |c| backups::list(c, &instance_id)).await
    }

    async fn get_backup(&self, instance_id: &str, id: &str) -> Result<Backup> {
        let (instance_id, id) = (instance_id.to_owned(), id.to_owned());
        self.run(move |c| backups::get(c, &instance_id, &id)).await
    }

    async fn delete_backups_for_instance(&self, instance_id: &str) -> Result<()> {
        let instance_id = instance_id.to_owned();
        self.run(move |c| backups::delete_for_instance(c, &instance_id))
            .await
    }

    async fn create_cron(&self, c: &Cron) -> Result<()> {
        let cron = c.clone();
        self.run(move |c| crons::create(c, &cron)).await
    }

    async fn get_cron(&self, instance_id: &str, id: &str) -> Result<Cron> {
        let (instance_id, id) = (instance_id.to_owned(), id.to_owned());
        self.run(move |c| crons::get(c, &instance_id, &id)).await
    }

    async fn list_crons(&self, instance_id: &str) -> Result<Vec<Cron>> {
        let instance_id = instance_id.to_owned();
        self.run(move |c| crons::list(c, &instance_id)).await
    }

    async fn list_due_crons(&self, now: i64) -> Result<Vec<Cron>> {
        self.run(move |c| crons::list_due(c, now)).await
    }

    async fn update_cron(&self, c: &Cron) -> Result<()> {
        let cron = c.clone();
        self.run(move |c| crons::update(c, &cron)).await
    }

    async fn delete_cron(&self, instance_id: &str, id: &str) -> Result<()> {
        let (instance_id, id) = (instance_id.to_owned(), id.to_owned());
        self.run(move |c| crons::delete(c, &instance_id, &id)).await
    }
}
