//! In-memory fakes for the traits the biller consumes: a `Store` that keeps
//! workspaces, instances and the ledger in maps, a settable `Clock` and an
//! `InstanceStopper` that records its calls. Test-only.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};

use async_trait::async_trait;
use parking_lot::Mutex;
use warren_core::{
    ApiKey, Backup, Clock, Cron, Error, GATEWAY_PORT, Instance, InstanceStatus, InstanceStopper,
    InstanceType, LedgerEntry, LedgerKind, MetricPoint, Money, Result, Shape, Store, Template,
    Workspace,
};

/// A lifecycle write applied to the instance map, see [`FakeStore::after_next_listing`].
type ListingHook = Box<dyn FnOnce(&mut BTreeMap<String, Instance>) + Send>;

#[derive(Default)]
struct State {
    workspaces: BTreeMap<String, Workspace>,
    instances: BTreeMap<String, Instance>,
    ledger: Vec<LedgerEntry>,
    next_id: u64,
    /// Run against the instance map right after the next `list_all_instances`
    /// has taken its snapshot, to simulate lifecycle writes that land between
    /// the biller's listing and its metering.
    after_listing: Vec<ListingHook>,
}

/// In-memory `Store`. Methods the biller does not use return `internal`.
#[derive(Default)]
pub struct FakeStore {
    state: Mutex<State>,
    /// Make `apply_ledger` fail.
    pub fail_ledger: AtomicBool,
    /// Make `set_meter` fail.
    pub fail_set_meter: AtomicBool,
}

fn unused() -> Error {
    Error::internal("not used by billing tests")
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

    pub fn workspace(&self, id: &str) -> Workspace {
        self.state
            .lock()
            .workspaces
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no workspace {id} in the fake store"))
    }

    pub fn instance(&self, id: &str) -> Instance {
        self.state
            .lock()
            .instances
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("no instance {id} in the fake store"))
    }

    /// Every entry, oldest first.
    pub fn ledger(&self) -> Vec<LedgerEntry> {
        self.state.lock().ledger.clone()
    }

    pub fn ledger_for_instance(&self, id: &str) -> Vec<LedgerEntry> {
        self.state
            .lock()
            .ledger
            .iter()
            .filter(|e| e.instance_id == id)
            .cloned()
            .collect()
    }

    /// Apply `f` to the instance map right after the biller's next listing,
    /// before it meters: the lifecycle writing between the two.
    pub fn after_next_listing(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, Instance>) + Send + 'static,
    ) {
        self.state.lock().after_listing.push(Box::new(f));
    }
}

#[async_trait]
impl Store for FakeStore {
    async fn create_workspace(&self, ws: &Workspace) -> Result<()> {
        let mut st = self.state.lock();
        if st.workspaces.contains_key(&ws.id) {
            return Err(Error::conflict("workspace exists"));
        }
        st.workspaces.insert(ws.id.clone(), ws.clone());
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

    async fn create_api_key(&self, _k: &ApiKey) -> Result<()> {
        Err(unused())
    }
    async fn get_api_key_by_hash(&self, _hash: &str) -> Result<ApiKey> {
        Err(unused())
    }
    async fn list_api_keys(&self, _workspace_id: &str) -> Result<Vec<ApiKey>> {
        Err(unused())
    }
    async fn delete_api_key(&self, _workspace_id: &str, _id: &str) -> Result<()> {
        Err(unused())
    }
    async fn touch_api_key(&self, _id: &str, _at: i64) -> Result<()> {
        Err(unused())
    }

    async fn upsert_template(&self, _t: &Template) -> Result<()> {
        Err(unused())
    }
    async fn get_template(&self, _workspace_id: &str, _name: &str) -> Result<Template> {
        Err(unused())
    }
    async fn get_template_revision(
        &self,
        _workspace_id: &str,
        _name: &str,
        _revision: u32,
    ) -> Result<Template> {
        Err(unused())
    }
    async fn list_templates(&self, _workspace_id: &str) -> Result<Vec<Template>> {
        Err(unused())
    }
    async fn delete_template(&self, _workspace_id: &str, _name: &str) -> Result<()> {
        Err(unused())
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
        let inst = self.get_instance(id).await?;
        if inst.workspace_id != workspace_id {
            return Err(Error::not_found("instance"));
        }
        Ok(inst)
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
        let all: Vec<Instance> = {
            let mut st = self.state.lock();
            let all = st.instances.values().cloned().collect();
            let hooks = std::mem::take(&mut st.after_listing);
            for hook in hooks {
                hook(&mut st.instances);
            }
            all
        };
        // Give concurrent callers a chance to interleave, as a real database would.
        tokio::task::yield_now().await;
        Ok(all)
    }
    /// Like the real store, writes every field except the meter position.
    async fn update_instance(&self, inst: &Instance) -> Result<()> {
        let mut st = self.state.lock();
        let Some(stored) = st.instances.get_mut(&inst.id) else {
            return Err(Error::not_found("instance"));
        };
        let (metered_through, meter_carry_micros) =
            (stored.metered_through, stored.meter_carry_micros);
        *stored = inst.clone();
        stored.metered_through = metered_through;
        stored.meter_carry_micros = meter_carry_micros;
        Ok(())
    }
    async fn set_meter(
        &self,
        id: &str,
        metered_through: i64,
        carry_micros: f64,
        expected_from: Option<i64>,
    ) -> Result<bool> {
        if self.fail_set_meter.load(Ordering::SeqCst) {
            return Err(Error::internal("instance table unavailable (injected)"));
        }
        let mut st = self.state.lock();
        let Some(stored) = st.instances.get_mut(id) else {
            return Err(Error::not_found("instance"));
        };
        if expected_from.is_some_and(|expected| stored.metered_through != expected) {
            return Ok(false);
        }
        stored.metered_through = metered_through;
        stored.meter_carry_micros = carry_micros;
        Ok(true)
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
        Ok(self
            .state
            .lock()
            .instances
            .values()
            .filter(|i| i.workspace_id == workspace_id && i.status.awake())
            .count() as u32)
    }
    async fn count_instances_using_template(
        &self,
        _workspace_id: &str,
        _name: &str,
    ) -> Result<u32> {
        Err(unused())
    }

    async fn apply_ledger(&self, entry: &mut LedgerEntry) -> Result<()> {
        if self.fail_ledger.load(Ordering::SeqCst) {
            return Err(Error::internal("ledger unavailable (injected)"));
        }
        let mut st = self.state.lock();
        if !st.workspaces.contains_key(&entry.workspace_id) {
            return Err(Error::not_found("workspace"));
        }
        if entry.id.is_empty() {
            st.next_id += 1;
            entry.id = format!("{:032x}", st.next_id);
        }
        let ws = st
            .workspaces
            .get_mut(&entry.workspace_id)
            .expect("checked above");
        ws.wallet_micros += entry.micros;
        if entry.kind == LedgerKind::Topup {
            ws.top_up_total_micros += entry.micros;
        }
        st.ledger.push(entry.clone());
        Ok(())
    }
    async fn list_ledger(&self, workspace_id: &str, limit: u32) -> Result<Vec<LedgerEntry>> {
        Ok(self
            .state
            .lock()
            .ledger
            .iter()
            .rev()
            .filter(|e| e.workspace_id == workspace_id)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn insert_metric(&self, _instance_id: &str, _p: MetricPoint) -> Result<()> {
        Err(unused())
    }
    async fn list_metrics(&self, _instance_id: &str, _since: i64) -> Result<Vec<MetricPoint>> {
        Err(unused())
    }
    async fn prune_metrics(&self, _before: i64) -> Result<()> {
        Err(unused())
    }

    async fn create_backup(&self, _b: &Backup) -> Result<()> {
        Err(unused())
    }
    async fn list_backups(&self, _instance_id: &str) -> Result<Vec<Backup>> {
        Err(unused())
    }
    async fn get_backup(&self, _instance_id: &str, _id: &str) -> Result<Backup> {
        Err(unused())
    }
    async fn delete_backups_for_instance(&self, _instance_id: &str) -> Result<()> {
        Err(unused())
    }

    async fn create_cron(&self, _c: &Cron) -> Result<()> {
        Err(unused())
    }
    async fn get_cron(&self, _instance_id: &str, _id: &str) -> Result<Cron> {
        Err(unused())
    }
    async fn list_crons(&self, _instance_id: &str) -> Result<Vec<Cron>> {
        Err(unused())
    }
    async fn list_due_crons(&self, _now: i64) -> Result<Vec<Cron>> {
        Err(unused())
    }
    async fn update_cron(&self, _c: &Cron) -> Result<()> {
        Err(unused())
    }
    async fn delete_cron(&self, _instance_id: &str, _id: &str) -> Result<()> {
        Err(unused())
    }
}

/// A clock the test moves by hand.
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn new(now: i64) -> Arc<Self> {
        Arc::new(FixedClock(AtomicI64::new(now)))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Records `(instance_id, reason)` for every stop request; can be told to fail
/// for one instance.
#[derive(Default)]
pub struct RecordingStopper {
    calls: Mutex<Vec<(String, String)>>,
    fail_for: Mutex<Option<String>>,
}

impl RecordingStopper {
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().clone()
    }
    pub fn fail_for(&self, instance_id: &str) {
        *self.fail_for.lock() = Some(instance_id.to_string());
    }
}

#[async_trait]
impl InstanceStopper for RecordingStopper {
    async fn stop_for_billing(&self, instance_id: &str, reason: &str) -> Result<()> {
        self.calls
            .lock()
            .push((instance_id.to_string(), reason.to_string()));
        if self.fail_for.lock().as_deref() == Some(instance_id) {
            return Err(Error::conflict("injected stop failure"));
        }
        Ok(())
    }
}

pub fn workspace(id: &str, wallet_micros: Money, top_up_total_micros: Money) -> Workspace {
    Workspace {
        id: id.into(),
        name: id.into(),
        created_at: 0,
        wallet_micros,
        top_up_total_micros,
        free_credit_granted: false,
    }
}

/// An instance whose meter started (and was last metered) at `meter_started_at`;
/// pass 0 for one that never reached running.
pub fn instance(
    id: &str,
    workspace_id: &str,
    status: InstanceStatus,
    shape: Shape,
    kind: InstanceType,
    meter_started_at: i64,
) -> Instance {
    Instance {
        id: id.into(),
        workspace_id: workspace_id.into(),
        template: "warren-base".into(),
        template_name: "warren-base".into(),
        template_revision: 1,
        image_ref: "warren/base:dev".into(),
        kind,
        status,
        shape,
        region: "local".into(),
        default_port: GATEWAY_PORT,
        mount_path: "/home/agent".into(),
        runtime: "docker".into(),
        meter_started_at,
        metered_through: meter_started_at,
        ..Default::default()
    }
}
