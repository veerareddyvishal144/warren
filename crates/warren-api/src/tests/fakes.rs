//! In-memory implementations of the traits `Deps` consumes.
//!
//! `async-trait` is not a dependency of this crate, so the `boxed!` macro
//! spells out the signature `#[async_trait]` generates: one lifetime per
//! reference argument, all outliving `'async_trait`, and a pinned boxed
//! `Send` future.

use std::{
    collections::BTreeMap,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicI64, Ordering},
    },
};

use warren_core::{
    ApiKey, Backup, Biller, Clock, Config, CreateInstanceRequest, Cron, DEFAULT_IDLE_TIMEOUT_SEC,
    Error, ExecRequest, ExecResult, ForkRequest, Instance, InstanceStatus, InstanceType,
    LedgerEntry, LedgerKind, Lifecycle, LogsOptions, MICROS_PER_DOLLAR, MINUTES_PER_MONTH,
    MetricPoint, Money, PatchInstanceRequest, Rates, RegisterTemplateRequest, Result, Shape,
    ShapeRequest, Store, Template, TemplateScope, TemplateService, Workspace, ids, shapes,
};

macro_rules! boxed {
    ($( fn $name:ident <$($lt:lifetime),*> ($($params:tt)*) -> $out:ty $body:block )*) => {
        $(
            fn $name<$($lt,)* 'async_trait>($($params)*)
                -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = $out> + Send + 'async_trait>>
            where
                $($lt: 'async_trait,)*
                Self: 'async_trait,
            {
                Box::pin(async move $body)
            }
        )*
    };
}

/// A clock that only moves when a test says so.
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn new(now: i64) -> Self {
        FixedClock(AtomicI64::new(now))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct Tables {
    pub workspaces: BTreeMap<String, Workspace>,
    pub keys: Vec<ApiKey>,
    /// Every revision of every template; system templates have an empty workspace id.
    pub templates: Vec<Template>,
    pub instances: BTreeMap<String, Instance>,
    pub ledger: Vec<LedgerEntry>,
    pub metrics: Vec<(String, MetricPoint)>,
    pub backups: Vec<Backup>,
    pub crons: Vec<Cron>,
}

/// `Store` on top of a mutex-guarded set of tables.
#[derive(Default)]
pub struct MemStore {
    tables: Mutex<Tables>,
}

impl MemStore {
    pub fn lock(&self) -> MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn visible(t: &Template, workspace_id: &str) -> bool {
        t.workspace_id.is_empty() || t.workspace_id == workspace_id
    }

    /// Latest revision of a template visible to the workspace.
    fn latest(tables: &Tables, workspace_id: &str, name: &str) -> Option<Template> {
        tables
            .templates
            .iter()
            .filter(|t| t.name == name && Self::visible(t, workspace_id))
            .max_by_key(|t| t.revision)
            .cloned()
    }
}

impl Store for MemStore {
    boxed! {
        fn create_workspace<'a, 'b>(&'a self, ws: &'b Workspace) -> Result<()> {
            let mut t = self.lock();
            if t.workspaces.contains_key(&ws.id) {
                return Err(Error::conflict("workspace exists"));
            }
            t.workspaces.insert(ws.id.clone(), ws.clone());
            Ok(())
        }
        fn get_workspace<'a, 'b>(&'a self, id: &'b str) -> Result<Workspace> {
            self.lock().workspaces.get(id).cloned().ok_or_else(|| Error::not_found("workspace"))
        }
        fn update_workspace<'a, 'b>(&'a self, ws: &'b Workspace) -> Result<()> {
            let mut t = self.lock();
            if !t.workspaces.contains_key(&ws.id) {
                return Err(Error::not_found("workspace"));
            }
            t.workspaces.insert(ws.id.clone(), ws.clone());
            Ok(())
        }
        fn list_workspaces<'a>(&'a self) -> Result<Vec<Workspace>> {
            Ok(self.lock().workspaces.values().cloned().collect())
        }

        fn create_api_key<'a, 'b>(&'a self, k: &'b ApiKey) -> Result<()> {
            self.lock().keys.push(k.clone());
            Ok(())
        }
        fn get_api_key_by_hash<'a, 'b>(&'a self, hash: &'b str) -> Result<ApiKey> {
            self.lock().keys.iter().find(|k| k.hash == hash).cloned().ok_or_else(|| Error::not_found("api key"))
        }
        fn list_api_keys<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<Vec<ApiKey>> {
            Ok(self.lock().keys.iter().filter(|k| k.workspace_id == workspace_id).cloned().collect())
        }
        fn delete_api_key<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<()> {
            let mut t = self.lock();
            let before = t.keys.len();
            t.keys.retain(|k| !(k.workspace_id == workspace_id && k.id == id));
            if t.keys.len() == before { Err(Error::not_found("api key")) } else { Ok(()) }
        }
        fn touch_api_key<'a, 'b>(&'a self, id: &'b str, at: i64) -> Result<()> {
            if let Some(k) = self.lock().keys.iter_mut().find(|k| k.id == id) {
                k.last_used_at = at;
            }
            Ok(())
        }

        fn upsert_template<'a, 'b>(&'a self, tpl: &'b Template) -> Result<()> {
            let mut t = self.lock();
            let current = t
                .templates
                .iter()
                .filter(|x| x.name == tpl.name && x.workspace_id == tpl.workspace_id)
                .map(|x| x.revision)
                .max()
                .unwrap_or(0);
            let mut tpl = tpl.clone();
            tpl.revision = current + 1;
            t.templates.push(tpl);
            Ok(())
        }
        fn get_template<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<Template> {
            Self::latest(&self.lock(), workspace_id, name).ok_or_else(|| Error::not_found("template"))
        }
        fn get_template_revision<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str, revision: u32) -> Result<Template> {
            self.lock()
                .templates
                .iter()
                .find(|t| t.name == name && t.revision == revision && Self::visible(t, workspace_id))
                .cloned()
                .ok_or_else(|| Error::not_found("template revision"))
        }
        fn list_templates<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<Vec<Template>> {
            let t = self.lock();
            let mut names: Vec<(String, String)> =
                t.templates.iter().filter(|x| Self::visible(x, workspace_id)).map(|x| (x.workspace_id.clone(), x.name.clone())).collect();
            names.sort();
            names.dedup();
            Ok(names.into_iter().filter_map(|(ws, name)| Self::latest(&t, if ws.is_empty() { workspace_id } else { &ws }, &name)).collect())
        }
        fn delete_template<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<()> {
            let mut t = self.lock();
            let before = t.templates.len();
            t.templates.retain(|x| !(x.name == name && x.workspace_id == workspace_id && !workspace_id.is_empty()));
            if t.templates.len() == before { Err(Error::not_found("template")) } else { Ok(()) }
        }

        fn create_instance<'a, 'b>(&'a self, inst: &'b Instance) -> Result<()> {
            let mut t = self.lock();
            if t.instances.contains_key(&inst.id) {
                return Err(Error::conflict("instance exists"));
            }
            t.instances.insert(inst.id.clone(), inst.clone());
            Ok(())
        }
        fn get_instance<'a, 'b>(&'a self, id: &'b str) -> Result<Instance> {
            self.lock().instances.get(id).cloned().ok_or_else(|| Error::not_found("instance"))
        }
        fn get_instance_for_workspace<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.lock()
                .instances
                .get(id)
                .filter(|i| i.workspace_id == workspace_id)
                .cloned()
                .ok_or_else(|| Error::not_found("instance"))
        }
        fn list_instances<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<Vec<Instance>> {
            Ok(self.lock().instances.values().filter(|i| i.workspace_id == workspace_id).cloned().collect())
        }
        fn list_all_instances<'a>(&'a self) -> Result<Vec<Instance>> {
            Ok(self.lock().instances.values().cloned().collect())
        }
        fn update_instance<'a, 'b>(&'a self, inst: &'b Instance) -> Result<()> {
            let mut t = self.lock();
            if !t.instances.contains_key(&inst.id) {
                return Err(Error::not_found("instance"));
            }
            t.instances.insert(inst.id.clone(), inst.clone());
            Ok(())
        }
        fn delete_instance<'a, 'b>(&'a self, id: &'b str) -> Result<()> {
            self.lock().instances.remove(id).map(|_| ()).ok_or_else(|| Error::not_found("instance"))
        }
        fn count_awake_instances<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<u32> {
            Ok(self.lock().instances.values().filter(|i| i.workspace_id == workspace_id && i.status.awake()).count() as u32)
        }
        fn count_instances_using_template<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<u32> {
            Ok(self
                .lock()
                .instances
                .values()
                .filter(|i| i.workspace_id == workspace_id && i.template_name == name && i.status != InstanceStatus::Deleted)
                .count() as u32)
        }

        fn apply_ledger<'a, 'b>(&'a self, entry: &'b mut LedgerEntry) -> Result<()> {
            let mut t = self.lock();
            let ws = t.workspaces.get_mut(&entry.workspace_id).ok_or_else(|| Error::not_found("workspace"))?;
            ws.wallet_micros += entry.micros;
            if entry.kind == LedgerKind::Topup {
                ws.top_up_total_micros += entry.micros;
            }
            if entry.id.is_empty() {
                entry.id = ids::new_id();
            }
            t.ledger.push(entry.clone());
            Ok(())
        }
        fn list_ledger<'a, 'b>(&'a self, workspace_id: &'b str, limit: u32) -> Result<Vec<LedgerEntry>> {
            let t = self.lock();
            let mut out: Vec<LedgerEntry> = t.ledger.iter().rev().filter(|e| e.workspace_id == workspace_id).cloned().collect();
            out.sort_by_key(|e| std::cmp::Reverse(e.at));
            out.truncate(limit as usize);
            Ok(out)
        }

        fn insert_metric<'a, 'b>(&'a self, instance_id: &'b str, p: MetricPoint) -> Result<()> {
            self.lock().metrics.push((instance_id.to_string(), p));
            Ok(())
        }
        fn list_metrics<'a, 'b>(&'a self, instance_id: &'b str, since: i64) -> Result<Vec<MetricPoint>> {
            Ok(self.lock().metrics.iter().filter(|(id, p)| id == instance_id && p.at >= since).map(|(_, p)| *p).collect())
        }
        fn prune_metrics<'a>(&'a self, before: i64) -> Result<()> {
            self.lock().metrics.retain(|(_, p)| p.at >= before);
            Ok(())
        }

        fn create_backup<'a, 'b>(&'a self, b: &'b Backup) -> Result<()> {
            self.lock().backups.push(b.clone());
            Ok(())
        }
        fn list_backups<'a, 'b>(&'a self, instance_id: &'b str) -> Result<Vec<Backup>> {
            Ok(self.lock().backups.iter().filter(|b| b.instance_id == instance_id).cloned().collect())
        }
        fn get_backup<'a, 'b, 'c>(&'a self, instance_id: &'b str, id: &'c str) -> Result<Backup> {
            self.lock()
                .backups
                .iter()
                .find(|b| b.instance_id == instance_id && b.id == id)
                .cloned()
                .ok_or_else(|| Error::not_found("backup"))
        }
        fn delete_backups_for_instance<'a, 'b>(&'a self, instance_id: &'b str) -> Result<()> {
            self.lock().backups.retain(|b| b.instance_id != instance_id);
            Ok(())
        }

        fn create_cron<'a, 'b>(&'a self, c: &'b Cron) -> Result<()> {
            self.lock().crons.push(c.clone());
            Ok(())
        }
        fn get_cron<'a, 'b, 'c>(&'a self, instance_id: &'b str, id: &'c str) -> Result<Cron> {
            self.lock()
                .crons
                .iter()
                .find(|c| c.instance_id == instance_id && c.id == id)
                .cloned()
                .ok_or_else(|| Error::not_found("cron"))
        }
        fn list_crons<'a, 'b>(&'a self, instance_id: &'b str) -> Result<Vec<Cron>> {
            Ok(self.lock().crons.iter().filter(|c| c.instance_id == instance_id).cloned().collect())
        }
        fn list_due_crons<'a>(&'a self, now: i64) -> Result<Vec<Cron>> {
            Ok(self.lock().crons.iter().filter(|c| c.enabled && c.next_run_at <= now).cloned().collect())
        }
        fn update_cron<'a, 'b>(&'a self, c: &'b Cron) -> Result<()> {
            let mut t = self.lock();
            let slot = t.crons.iter_mut().find(|x| x.id == c.id && x.instance_id == c.instance_id).ok_or_else(|| Error::not_found("cron"))?;
            *slot = c.clone();
            Ok(())
        }
        fn delete_cron<'a, 'b, 'c>(&'a self, instance_id: &'b str, id: &'c str) -> Result<()> {
            let mut t = self.lock();
            let before = t.crons.len();
            t.crons.retain(|c| !(c.instance_id == instance_id && c.id == id));
            if t.crons.len() == before { Err(Error::not_found("cron")) } else { Ok(()) }
        }
    }
}

/// `TemplateService` over the store: system catalog from the config, workspace
/// templates with revisions, `name@rev` pins.
pub struct FakeTemplates {
    store: std::sync::Arc<MemStore>,
    config: Config,
    clock: std::sync::Arc<FixedClock>,
}

impl FakeTemplates {
    pub fn new(
        store: std::sync::Arc<MemStore>,
        config: Config,
        clock: std::sync::Arc<FixedClock>,
    ) -> Self {
        FakeTemplates {
            store,
            config,
            clock,
        }
    }

    /// Seed the built-in catalog synchronously.
    pub fn seed(&self) {
        let mut t = self.store.lock();
        for tpl in self.config.system_templates() {
            if !t
                .templates
                .iter()
                .any(|x| x.name == tpl.name && x.workspace_id.is_empty())
            {
                t.templates.push(tpl);
            }
        }
    }
}

impl TemplateService for FakeTemplates {
    boxed! {
        fn resolve<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<Template> {
            match name.split_once('@') {
                Some((n, rev)) => {
                    let rev: u32 = rev.parse().map_err(|_| Error::invalid("bad revision"))?;
                    let tpl = self.store.get_template_revision(workspace_id, n, rev).await?;
                    if tpl.scope == TemplateScope::System && self.store.get_template(workspace_id, n).await?.revision != rev {
                        return Err(Error::not_found("template revision"));
                    }
                    Ok(tpl)
                }
                None => self.store.get_template(workspace_id, name).await,
            }
        }
        fn get<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<Template> {
            self.store.get_template(workspace_id, name).await
        }
        fn list<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<Vec<Template>> {
            let mut all = self.store.list_templates(workspace_id).await?;
            all.sort_by(|a, b| (a.scope != TemplateScope::System).cmp(&(b.scope != TemplateScope::System)).then_with(|| a.name.cmp(&b.name)));
            Ok(all)
        }
        fn register<'a, 'b, 'c>(&'a self, workspace_id: &'b str, req: &'c RegisterTemplateRequest) -> Result<Template> {
            if !ids::valid_template_name(&req.name) || req.name.starts_with("warren-") || req.image_ref.is_empty() {
                return Err(Error::invalid("bad template request"));
            }
            let now = self.clock.now_unix();
            let tpl = Template {
                name: req.name.clone(),
                scope: TemplateScope::Workspace,
                workspace_id: workspace_id.to_string(),
                image_ref: req.image_ref.clone(),
                image_digest: "sha256:fake".into(),
                default_port: req.default_port,
                mount_path: if req.mount_path.is_empty() { "/home/agent".into() } else { req.mount_path.clone() },
                agent: req.agent,
                description: req.description.clone(),
                types: vec![],
                default_shape: Shape { cpu: 2, memory: 4, disk: 4 },
                env: req.env.clone(),
                revision: 0,
                created_at: now,
                updated_at: now,
            };
            self.store.upsert_template(&tpl).await?;
            self.store.get_template(workspace_id, &req.name).await
        }
        fn delete<'a, 'b, 'c>(&'a self, workspace_id: &'b str, name: &'c str) -> Result<()> {
            if self.store.count_instances_using_template(workspace_id, name).await? > 0 {
                return Err(Error::conflict("template is in use"));
            }
            self.store.delete_template(workspace_id, name).await
        }
        fn ensure_system_templates<'a>(&'a self) -> Result<()> {
            self.seed();
            Ok(())
        }
    }
}

/// `Biller` with the documented rules: one day of balance to create, the
/// awake-limit ladder, dev top-up bounds.
pub struct FakeBiller {
    store: std::sync::Arc<MemStore>,
    clock: std::sync::Arc<FixedClock>,
    rates: Rates,
}

impl FakeBiller {
    pub fn new(store: std::sync::Arc<MemStore>, clock: std::sync::Arc<FixedClock>) -> Self {
        FakeBiller {
            store,
            clock,
            rates: Rates::default(),
        }
    }

    async fn check_awake(&self, ws: &Workspace) -> Result<()> {
        let awake = self.store.count_awake_instances(&ws.id).await?;
        let limit = self.awake_limit(ws);
        if awake >= limit {
            Err(Error::instance_limit(limit, awake))
        } else {
            Ok(())
        }
    }

    async fn apply(
        &self,
        workspace_id: &str,
        kind: LedgerKind,
        micros: Money,
        note: &str,
    ) -> Result<Workspace> {
        let mut entry = LedgerEntry {
            id: String::new(),
            workspace_id: workspace_id.to_string(),
            instance_id: String::new(),
            kind,
            micros,
            minutes: 0.0,
            at: self.clock.now_unix(),
            note: note.to_string(),
        };
        self.store.apply_ledger(&mut entry).await?;
        self.store.get_workspace(workspace_id).await
    }
}

impl Biller for FakeBiller {
    fn rates(&self) -> Rates {
        self.rates
    }
    fn monthly_running_micros(&self, shape: Shape, t: InstanceType) -> Money {
        self.rates.monthly_running(shape, t)
    }
    fn monthly_disk_micros(&self, shape: Shape) -> Money {
        self.rates.monthly_disk(shape)
    }
    fn awake_limit(&self, ws: &Workspace) -> u32 {
        let d = ws.top_up_total_micros / MICROS_PER_DOLLAR;
        match ws.top_up_total_micros {
            0 => 3,
            _ if d >= 1000 => 1000,
            _ if d >= 250 => 200,
            _ if d >= 50 => 50,
            _ => 10,
        }
    }
    boxed! {
        fn assert_can_create<'a, 'b>(&'a self, ws: &'b Workspace, shape: Shape, t: InstanceType) -> Result<()> {
            let per_minute = self.monthly_running_micros(shape, t) as f64 / MINUTES_PER_MONTH;
            let day = (per_minute * 1440.0).ceil() as Money;
            if ws.wallet_micros < day {
                return Err(Error::insufficient_balance(day, ws.wallet_micros));
            }
            self.check_awake(ws).await
        }
        fn assert_can_wake<'a, 'b>(&'a self, ws: &'b Workspace) -> Result<()> {
            self.check_awake(ws).await
        }
        fn top_up<'a, 'b, 'c>(&'a self, workspace_id: &'b str, micros: Money, note: &'c str) -> Result<Workspace> {
            if !(5 * MICROS_PER_DOLLAR..=10_000 * MICROS_PER_DOLLAR).contains(&micros) {
                return Err(Error::invalid("amount_micros must be between 5000000 and 10000000000"));
            }
            self.apply(workspace_id, LedgerKind::Topup, micros, note).await
        }
        fn grant_credit<'a, 'b, 'c>(&'a self, workspace_id: &'b str, micros: Money, note: &'c str) -> Result<Workspace> {
            self.apply(workspace_id, LedgerKind::Credit, micros, note).await
        }
        fn tick<'a>(&'a self, _now: i64) -> Result<()> {
            Ok(())
        }
    }
}

/// `Lifecycle` that records state in the store without touching a runtime.
/// `fail_next` injects one error into the next call; `calls` records the
/// methods invoked.
pub struct FakeLifecycle {
    store: std::sync::Arc<MemStore>,
    templates: std::sync::Arc<FakeTemplates>,
    biller: std::sync::Arc<FakeBiller>,
    config: Config,
    clock: std::sync::Arc<FixedClock>,
    pub fail_next: Mutex<Option<Error>>,
    pub calls: Mutex<Vec<String>>,
}

impl FakeLifecycle {
    pub fn new(
        store: std::sync::Arc<MemStore>,
        templates: std::sync::Arc<FakeTemplates>,
        biller: std::sync::Arc<FakeBiller>,
        config: Config,
        clock: std::sync::Arc<FixedClock>,
    ) -> Self {
        FakeLifecycle {
            store,
            templates,
            biller,
            config,
            clock,
            fail_next: Mutex::new(None),
            calls: Mutex::new(vec![]),
        }
    }

    pub fn fail_next_with(&self, e: Error) {
        *self.fail_next.lock().unwrap() = Some(e);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn enter(&self, name: &str) -> Result<()> {
        self.calls.lock().unwrap().push(name.to_string());
        match self.fail_next.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn transition(
        &self,
        workspace_id: &str,
        id: &str,
        status: InstanceStatus,
    ) -> Result<Instance> {
        let mut inst = self
            .store
            .get_instance_for_workspace(workspace_id, id)
            .await?;
        inst.status = status;
        inst.updated_at = self.clock.now_unix();
        self.store.update_instance(&inst).await?;
        Ok(inst)
    }
}

impl Lifecycle for FakeLifecycle {
    fn record_activity(&self, _id: &str) {}

    boxed! {
        fn create<'a, 'b, 'c>(&'a self, ws: &'b Workspace, req: &'c CreateInstanceRequest) -> Result<Instance> {
            self.enter("create")?;
            let template = req.template.clone().unwrap_or_else(|| "warren-base".to_string());
            let tpl = self.templates.resolve(&ws.id, &template).await?;
            let kind = req.kind.unwrap_or_default();
            if kind == InstanceType::Performance || !tpl.allows(kind) {
                return Err(Error::not_supported(format!("type {kind} is not available for template {}", tpl.name)));
            }
            let shape = shapes::normalize(req.shape.as_ref(), Some(&tpl), kind)?;
            self.biller.assert_can_create(ws, shape, kind).await?;
            let now = self.clock.now_unix();
            let inst = Instance {
                id: ids::new_instance_id(),
                workspace_id: ws.id.clone(),
                name: req.name.clone().unwrap_or_default(),
                template,
                template_name: tpl.name.clone(),
                template_revision: tpl.revision,
                image_ref: tpl.image_ref.clone(),
                kind,
                status: InstanceStatus::Provisioning,
                shape,
                region: req.region.clone().unwrap_or_else(|| self.config.region.clone()),
                metadata: req.metadata.clone().unwrap_or_default(),
                env: req.env.clone().unwrap_or_default(),
                network: req.network.unwrap_or_default(),
                auto_sleep: req.auto_sleep.unwrap_or(false),
                idle_timeout_seconds: req.idle_timeout_seconds.unwrap_or(DEFAULT_IDLE_TIMEOUT_SEC),
                budget: req.budget.unwrap_or_default(),
                default_port: tpl.default_port,
                mount_path: tpl.mount_path.clone(),
                agent_api: tpl.agent,
                created_at: now,
                updated_at: now,
                runtime: if kind == InstanceType::Vm { "firecracker".into() } else { "docker".into() },
                instance_token: ids::new_instance_token(),
                ..Default::default()
            };
            self.store.create_instance(&inst).await?;
            Ok(inst)
        }
        fn get<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.enter("get")?;
            self.store.get_instance_for_workspace(workspace_id, id).await
        }
        fn list<'a, 'b>(&'a self, workspace_id: &'b str) -> Result<Vec<Instance>> {
            self.enter("list")?;
            self.store.list_instances(workspace_id).await
        }
        fn patch<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, req: &'d PatchInstanceRequest) -> Result<Instance> {
            self.enter("patch")?;
            let mut inst = self.store.get_instance_for_workspace(workspace_id, id).await?;
            if let Some(n) = &req.name {
                inst.name = n.clone();
            }
            if let Some(m) = &req.metadata {
                inst.metadata = m.clone();
            }
            if let Some(e) = &req.env {
                inst.env = e.clone();
            }
            if let Some(a) = req.auto_sleep {
                inst.auto_sleep = a;
            }
            if let Some(t) = req.idle_timeout_seconds {
                inst.idle_timeout_seconds = t;
            }
            if let Some(n) = req.network {
                inst.network = n;
            }
            if let Some(b) = req.budget {
                inst.budget = b;
            }
            inst.updated_at = self.clock.now_unix();
            self.store.update_instance(&inst).await?;
            Ok(inst)
        }
        fn start<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.enter("start")?;
            self.transition(workspace_id, id, InstanceStatus::Starting).await
        }
        fn stop<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.enter("stop")?;
            self.transition(workspace_id, id, InstanceStatus::Stopping).await
        }
        fn sleep<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.enter("sleep")?;
            self.transition(workspace_id, id, InstanceStatus::Sleeping).await
        }
        fn restart<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Instance> {
            self.enter("restart")?;
            self.transition(workspace_id, id, InstanceStatus::Restarting).await
        }
        fn delete<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<()> {
            self.enter("delete")?;
            self.store.get_instance_for_workspace(workspace_id, id).await?;
            self.store.delete_instance(id).await
        }
        fn resize<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, shape: &'d ShapeRequest) -> Result<Instance> {
            self.enter("resize")?;
            let mut inst = self.store.get_instance_for_workspace(workspace_id, id).await?;
            if let Some(c) = shape.cpu {
                inst.shape.cpu = c;
            }
            if let Some(m) = shape.memory {
                inst.shape.memory = m;
            }
            if let Some(d) = shape.disk {
                if d < inst.shape.disk {
                    return Err(Error::invalid("disk may only grow"));
                }
                inst.shape.disk = d;
            }
            self.store.update_instance(&inst).await?;
            Ok(inst)
        }
        fn update<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, template: Option<&'d str>) -> Result<Instance> {
            self.enter("update")?;
            let mut inst = self.store.get_instance_for_workspace(workspace_id, id).await?;
            let target = template.map(str::to_string).unwrap_or_else(|| inst.template_name.clone());
            let tpl = self.templates.resolve(workspace_id, &target).await?;
            inst.template = target;
            inst.template_name = tpl.name;
            inst.template_revision = tpl.revision;
            inst.image_ref = tpl.image_ref;
            inst.status = InstanceStatus::Updating;
            self.store.update_instance(&inst).await?;
            Ok(inst)
        }
        fn fork<'a, 'b, 'c, 'd>(&'a self, ws: &'b Workspace, id: &'c str, req: &'d ForkRequest) -> Result<Instance> {
            self.enter("fork")?;
            let src = self.store.get_instance_for_workspace(&ws.id, id).await?;
            let now = self.clock.now_unix();
            let mut inst = Instance {
                id: ids::new_instance_id(),
                name: req.name.clone().unwrap_or_else(|| format!("{} (fork)", src.name)),
                status: InstanceStatus::Provisioning,
                parent_id: src.id.clone(),
                created_at: now,
                updated_at: now,
                started_at: 0,
                stopped_at: 0,
                slept_at: 0,
                last_activity_at: 0,
                meter_started_at: 0,
                metered_through: 0,
                meter_carry_micros: 0.0,
                runtime_ref: String::new(),
                ip: String::new(),
                instance_token: ids::new_instance_token(),
                ..src.clone()
            };
            if let Some(m) = &req.metadata {
                inst.metadata = m.clone();
            }
            if let Some(a) = req.auto_sleep {
                inst.auto_sleep = a;
            }
            if let Some(t) = req.idle_timeout_seconds {
                inst.idle_timeout_seconds = t;
            }
            self.store.create_instance(&inst).await?;
            Ok(inst)
        }
        fn exec<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, req: &'d ExecRequest) -> Result<ExecResult> {
            self.enter("exec")?;
            let inst = self.store.get_instance_for_workspace(workspace_id, id).await?;
            if inst.status == InstanceStatus::Stopped {
                return Err(Error::conflict("instance is stopped"));
            }
            Ok(ExecResult { exit_code: 0, stdout: format!("ran: {}", req.command), stderr: String::new(), duration_ms: 1, truncated: false })
        }
        fn logs<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str, opts: LogsOptions) -> Result<String> {
            self.enter("logs")?;
            self.store.get_instance_for_workspace(workspace_id, id).await?;
            Ok(format!("tail={} since={}\n", opts.tail, opts.since))
        }
        fn metrics<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str, hours: u32) -> Result<Vec<MetricPoint>> {
            self.enter("metrics")?;
            self.store.get_instance_for_workspace(workspace_id, id).await?;
            self.store.list_metrics(id, self.clock.now_unix() - hours as i64 * 3600).await
        }
        fn backup<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, note: &'d str) -> Result<Backup> {
            self.enter("backup")?;
            self.store.get_instance_for_workspace(workspace_id, id).await?;
            let b = Backup {
                id: ids::new_id(),
                instance_id: id.to_string(),
                workspace_id: workspace_id.to_string(),
                path: String::new(),
                size_bytes: 42,
                created_at: self.clock.now_unix(),
                note: note.to_string(),
            };
            self.store.create_backup(&b).await?;
            Ok(b)
        }
        fn list_backups<'a, 'b, 'c>(&'a self, workspace_id: &'b str, id: &'c str) -> Result<Vec<Backup>> {
            self.enter("list_backups")?;
            self.store.get_instance_for_workspace(workspace_id, id).await?;
            self.store.list_backups(id).await
        }
        fn restore<'a, 'b, 'c, 'd>(&'a self, workspace_id: &'b str, id: &'c str, backup_id: &'d str) -> Result<Instance> {
            self.enter("restore")?;
            let inst = self.store.get_instance_for_workspace(workspace_id, id).await?;
            self.store.get_backup(id, backup_id).await?;
            Ok(inst)
        }
        fn ensure_awake<'a, 'b>(&'a self, id: &'b str) -> Result<Instance> {
            self.enter("ensure_awake")?;
            self.store.get_instance(id).await
        }
        fn endpoint<'a, 'b>(&'a self, id: &'b str) -> Result<String> {
            self.enter("endpoint")?;
            self.store.get_instance(id).await.map(|_| "172.18.0.2".to_string())
        }
    }
}
