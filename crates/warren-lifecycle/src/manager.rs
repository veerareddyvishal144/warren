//! The [`Manager`]: dependencies, per-instance locks, the activity map and the
//! small persistence helpers every operation shares.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::sync::OwnedMutexGuard;
use warren_core::{
    BrokerRegistry, Clock, Config, Error, Instance, InstanceStatus, InstanceType,
    MAX_IDLE_TIMEOUT_SEC, Result, Runtime, Runtimes, Store, Template, TemplateService,
};

/// Everything the lifecycle needs from the rest of the daemon.
pub struct Deps {
    pub store: Arc<dyn Store>,
    pub runtimes: Runtimes,
    pub templates: Arc<dyn TemplateService>,
    pub config: Config,
    pub clock: Arc<dyn Clock>,
    /// The egress broker's table of running instances; `None` when the daemon
    /// runs without a broker, which refuses `allowlist` instances and secrets.
    pub broker: Option<Arc<dyn BrokerRegistry>>,
}

/// Orchestrates instances. Construct with [`Manager::new`], drive the
/// background loops with [`Manager::run`].
pub struct Manager {
    pub(crate) deps: Deps,
    /// One async mutex per instance id; operations on one instance serialize.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One async mutex per workspace id, held from the awake-quota check
    /// through the insert or transition that consumes the slot, so concurrent
    /// create/start/wake/fork requests cannot exceed `max_awake_instances`.
    ws_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Instance id -> last activity (unix seconds), flushed to the store periodically.
    activity: Mutex<HashMap<String, i64>>,
    /// How long `ensure_awake` waits for a transitional instance, in milliseconds.
    wake_deadline_ms: AtomicU64,
    /// Lets `&self` methods hand an `Arc<Manager>` to spawned tasks.
    me: Weak<Manager>,
}

/// Longest status reason stored on an instance.
const MAX_REASON_CHARS: usize = 500;

impl Manager {
    /// Validate deps and return the manager. Background loops start in `run`.
    pub fn new(d: Deps) -> Result<Arc<Manager>> {
        if d.runtimes.is_empty() {
            return Err(Error::invalid("lifecycle needs at least one runtime"));
        }
        if d.config.min_idle_timeout_sec > MAX_IDLE_TIMEOUT_SEC {
            return Err(Error::invalid(format!(
                "min_idle_timeout_sec {} exceeds the maximum idle timeout {MAX_IDLE_TIMEOUT_SEC}",
                d.config.min_idle_timeout_sec
            )));
        }
        Ok(Arc::new_cyclic(|me| Manager {
            deps: d,
            locks: Mutex::new(HashMap::new()),
            ws_locks: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            wake_deadline_ms: AtomicU64::new(crate::ops::WAKE_DEADLINE.as_millis() as u64),
            me: me.clone(),
        }))
    }

    /// How long `ensure_awake` waits for an instance that is starting, waking,
    /// provisioning, restarting or updating before answering `timeout`.
    pub fn wake_deadline(&self) -> Duration {
        Duration::from_millis(self.wake_deadline_ms.load(Ordering::Relaxed))
    }

    /// Change the `ensure_awake` wait; the default is 90 seconds.
    pub fn set_wake_deadline(&self, d: Duration) {
        self.wake_deadline_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// The daemon configuration this manager was built with.
    pub fn config(&self) -> &Config {
        &self.deps.config
    }

    pub(crate) fn arc(&self) -> Result<Arc<Manager>> {
        self.me
            .upgrade()
            .ok_or_else(|| Error::internal("lifecycle manager was dropped"))
    }

    pub(crate) fn now(&self) -> i64 {
        self.deps.clock.now_unix()
    }

    fn lock_handle(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Wait for exclusive access to one instance.
    pub(crate) async fn lock(&self, id: &str) -> OwnedMutexGuard<()> {
        self.lock_handle(id).lock_owned().await
    }

    /// Exclusive access to one instance if nobody holds it right now.
    pub(crate) fn try_lock(&self, id: &str) -> Option<OwnedMutexGuard<()>> {
        self.lock_handle(id).try_lock_owned().ok()
    }

    /// Serialize the awake-quota check and the slot-consuming write for one
    /// workspace. Always taken after the instance lock (or before a brand-new
    /// instance's lock, which nobody else can hold), so the two never cycle.
    pub(crate) async fn ws_lock(&self, workspace_id: &str) -> OwnedMutexGuard<()> {
        let handle = self
            .ws_locks
            .lock()
            .entry(workspace_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        handle.lock_owned().await
    }

    /// Refuse with `instance_limit` when the workspace already keeps
    /// `Config::max_awake_instances` instances awake; `None` means unlimited.
    /// The caller holds [`Self::ws_lock`] from this check through the write
    /// that consumes the slot.
    pub(crate) async fn check_awake_quota(&self, workspace_id: &str) -> Result<()> {
        if let Some(limit) = self.deps.config.max_awake_instances {
            let awake = self.deps.store.count_awake_instances(workspace_id).await?;
            if awake >= limit {
                return Err(Error::instance_limit(limit, awake));
            }
        }
        Ok(())
    }

    /// Run `op` on a detached task that owns the instance lock, and wait for it.
    /// When the caller is dropped (the client disconnected mid-request) the
    /// task still runs to its end, so a transitional status written before a
    /// runtime call is always followed by the matching final state. Waiting
    /// for the lock stays cancellable because nothing has been written yet.
    pub(crate) async fn detached<T, F, Fut>(&self, id: &str, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Manager>, OwnedMutexGuard<()>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let me = self.arc()?;
        let guard = self.lock(id).await;
        match tokio::spawn(op(me, guard)).await {
            Ok(result) => result,
            Err(e) => Err(Error::internal(format!("lifecycle task aborted: {e}"))),
        }
    }

    pub(crate) fn forget(&self, id: &str) {
        self.locks.lock().remove(id);
        self.activity.lock().remove(id);
    }

    /// Note activity at `at` unless a later one is already recorded.
    pub(crate) fn touch_activity(&self, id: &str, at: i64) {
        let mut m = self.activity.lock();
        let e = m.entry(id.to_string()).or_insert(at);
        if at > *e {
            *e = at;
        }
    }

    pub(crate) fn activity_of(&self, id: &str) -> Option<i64> {
        self.activity.lock().get(id).copied()
    }

    /// Take every pending activity entry; the flush loop puts back what it could not store.
    pub(crate) fn drain_activity(&self) -> HashMap<String, i64> {
        std::mem::take(&mut *self.activity.lock())
    }

    /// Last activity as the rest of the system should see it: the later of the
    /// stored value and the in-memory one. Also fills the derived URL.
    pub(crate) fn overlay(&self, inst: &mut Instance) {
        if let Some(at) = self.activity_of(&inst.id)
            && at > inst.last_activity_at
        {
            inst.last_activity_at = at;
        }
        if inst.url.is_empty() {
            inst.url = self.deps.config.instance_url(&inst.id);
        }
    }

    pub(crate) fn runtime_for_kind(&self, kind: InstanceType) -> Result<Arc<dyn Runtime>> {
        self.deps
            .runtimes
            .get(&kind)
            .cloned()
            .ok_or_else(|| Error::not_supported(format!("no runtime for type {kind}")))
    }

    pub(crate) fn runtime_for(&self, inst: &Instance) -> Result<Arc<dyn Runtime>> {
        self.runtime_for_kind(inst.kind)
    }

    /// The instance scoped to a workspace; `not_found` when it belongs elsewhere.
    pub(crate) async fn load(&self, workspace_id: &str, id: &str) -> Result<Instance> {
        self.deps
            .store
            .get_instance_for_workspace(workspace_id, id)
            .await
    }

    /// Persist `inst`, stamping `updated_at`.
    pub(crate) async fn save(&self, inst: &mut Instance) -> Result<()> {
        inst.updated_at = self.now();
        self.deps.store.update_instance(inst).await
    }

    /// Move `inst` to `status` with `reason` and persist it.
    pub(crate) async fn transition(
        &self,
        inst: &mut Instance,
        status: InstanceStatus,
        reason: &str,
    ) -> Result<()> {
        let from = inst.status;
        inst.status = status;
        inst.status_reason = truncate(reason, MAX_REASON_CHARS);
        if from != status {
            tracing::info!(instance = %inst.id, from = %from, to = %status, reason, "status");
        }
        self.save(inst).await
    }

    /// Record a failure on the instance: status `Failed`, reason from the error.
    /// Errors while saving are logged, never surfaced, because the caller is
    /// already returning the original error.
    pub(crate) async fn fail(&self, inst: &mut Instance, err: &Error, what: &str) {
        let reason = describe(err);
        tracing::error!(instance = %inst.id, op = what, error = %reason, "operation failed");
        if let Err(e) = self.transition(inst, InstanceStatus::Failed, &reason).await {
            tracing::error!(instance = %inst.id, error = %e, "could not record failure");
        }
    }

    /// A stop or destroy failed. A `Failed` row holds no awake slot, so the
    /// runtime object must not keep running behind it: force it down with
    /// `destroy(remove_storage = false)` (the disk stays), then record both the
    /// original error and the outcome of the forced destroy as the reason.
    pub(crate) async fn fail_force(&self, inst: &mut Instance, err: &Error, what: &str) {
        let outcome = match self.runtime_for(inst) {
            Ok(rt) => match rt.destroy(inst, false).await {
                Ok(()) => {
                    inst.ip.clear();
                    "runtime destroyed".to_string()
                }
                Err(e) if e.is_not_found() => {
                    inst.ip.clear();
                    "runtime already gone".to_string()
                }
                Err(e) => format!("forced destroy failed: {}", describe(&e)),
            },
            Err(e) => format!("no runtime to destroy: {}", describe(&e)),
        };
        let reason = format!("{}; {outcome}", describe(err));
        tracing::error!(instance = %inst.id, op = what, error = %reason, "operation failed");
        if let Err(e) = self.transition(inst, InstanceStatus::Failed, &reason).await {
            tracing::error!(instance = %inst.id, error = %e, "could not record failure");
        }
    }

    /// The template an existing instance was built from, with the instance's
    /// own image and ports so a recreate keeps exactly what it ran. Resolves
    /// the pinned revision first and falls back to the current one.
    pub(crate) async fn effective_template(&self, inst: &Instance) -> Result<Template> {
        let ws = inst.workspace_id.as_str();
        let pinned = format!("{}@{}", inst.template_name, inst.template_revision);
        let mut tpl = match self.deps.templates.resolve(ws, &pinned).await {
            Ok(t) => t,
            Err(_) => self.deps.templates.resolve(ws, &inst.template_name).await?,
        };
        if !inst.image_ref.is_empty() {
            tpl.image_ref = inst.image_ref.clone();
        }
        if !inst.mount_path.is_empty() {
            tpl.mount_path = inst.mount_path.clone();
        }
        tpl.default_port = inst.default_port;
        tpl.agent = inst.agent_api;
        Ok(tpl)
    }
}

/// Human-readable reason for a failure, including the cause of internal errors.
pub(crate) fn describe(err: &Error) -> String {
    let cause = err
        .details
        .as_ref()
        .and_then(|d| d.get("cause"))
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty());
    match cause {
        Some(c) if err.is_internal() => c.to_string(),
        Some(c) => format!("{}: {c}", err.message),
        None => err.message.clone(),
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// `Error::conflict` naming the state that blocks the operation.
pub(crate) fn conflict_for(inst: &Instance) -> Error {
    if inst.status == InstanceStatus::Failed && !inst.status_reason.is_empty() {
        Error::conflict(format!("instance is failed: {}", inst.status_reason))
    } else {
        Error::conflict(format!("instance is {}", inst.status))
    }
}
