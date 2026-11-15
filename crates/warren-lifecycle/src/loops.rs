//! Startup reconciliation and the background loops driven by [`Manager::run`].
//!
//! Each loop body is a public method so an integrator or a test can trigger
//! one pass directly; `run` only schedules them until the token cancels.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use warren_core::{Instance, InstanceStatus, MetricPoint, Result, Runtime, STOP_GRACE_SECS};

use crate::manager::{Manager, describe};

/// Period of the idle sleeper.
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(10);
/// Period of the repair pass ([`Manager::repair`]).
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
/// A transitional row not updated for this long is considered stranded.
pub const STUCK_TRANSITION_SECS: i64 = 600;
/// Period of the metrics sampler.
pub const METRICS_INTERVAL: Duration = Duration::from_secs(60);
/// Period of the activity flush.
pub const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// Period of the disk usage refresh.
pub const DISK_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
/// Metrics older than this are pruned.
pub const METRICS_RETENTION_SECS: i64 = 7 * 86_400;
/// How often metrics are pruned.
pub const METRICS_PRUNE_INTERVAL_SECS: i64 = 3600;

/// Call `f` every `period` until `shutdown` cancels. The first call is immediate.
async fn every<F, Fut>(shutdown: CancellationToken, period: Duration, name: &'static str, f: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!(task = name, "loop stopped");
                return;
            }
            _ = interval.tick() => f().await,
        }
    }
}

impl Manager {
    /// Reconciler, idle sleeper, metrics sampler and disk refresh until shutdown.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        self.reconcile().await?;
        tracing::info!("lifecycle loops started");
        let mut tasks = tokio::task::JoinSet::new();

        let m = self.clone();
        tasks.spawn(every(
            shutdown.clone(),
            IDLE_SWEEP_INTERVAL,
            "idle sweeper",
            move || {
                let m = m.clone();
                async move { m.idle_sweep().await }
            },
        ));

        let m = self.clone();
        let last_prune = Arc::new(AtomicI64::new(0));
        tasks.spawn(every(
            shutdown.clone(),
            METRICS_INTERVAL,
            "metrics sampler",
            move || {
                let m = m.clone();
                let last_prune = last_prune.clone();
                async move {
                    m.sample_metrics().await;
                    let now = m.now();
                    if now - last_prune.load(Ordering::Relaxed) >= METRICS_PRUNE_INTERVAL_SECS {
                        last_prune.store(now, Ordering::Relaxed);
                        m.prune_metrics().await;
                    }
                }
            },
        ));

        let m = self.clone();
        tasks.spawn(every(
            shutdown.clone(),
            ACTIVITY_FLUSH_INTERVAL,
            "activity flush",
            move || {
                let m = m.clone();
                async move { m.flush_activity().await }
            },
        ));

        let m = self.clone();
        tasks.spawn(every(
            shutdown.clone(),
            DISK_REFRESH_INTERVAL,
            "disk usage refresh",
            move || {
                let m = m.clone();
                async move { m.refresh_disk_usage().await }
            },
        ));

        let m = self.clone();
        tasks.spawn(every(
            shutdown.clone(),
            RECONCILE_INTERVAL,
            "repair",
            move || {
                let m = m.clone();
                async move { m.repair().await }
            },
        ));

        shutdown.cancelled().await;
        while tasks.join_next().await.is_some() {}
        // Activity recorded since the last tick must not be lost on shutdown.
        self.flush_activity().await;
        tracing::info!("lifecycle stopped");
        Ok(())
    }

    /// Bring the stored state in line with what the runtimes actually run.
    /// Transitional states left by a crash become `Failed` (or finish, for
    /// `Stopping` and `Deleting`); running instances whose runtime is gone
    /// become `Stopped`; runtimes still running for non-running instances are
    /// stopped so nothing bills less than it uses. A runtime that cannot say
    /// whether an instance runs leaves the row untouched for the next pass.
    pub async fn reconcile(&self) -> Result<()> {
        let all = self.deps.store.list_all_instances().await?;
        let now = self.now();
        tracing::info!(instances = all.len(), "reconciling");
        for mut inst in all {
            let _g = self.lock(&inst.id).await;
            if let Err(e) = self.reconcile_one(&mut inst, now).await {
                tracing::error!(instance = %inst.id, status = %inst.status, error = %e, "reconcile failed");
            }
        }
        Ok(())
    }

    /// `Some(running)` from the runtime, `None` when it does not know; the
    /// caller must not change the row on `None`.
    async fn liveness(rt: &dyn Runtime, inst: &Instance, pass: &'static str) -> Option<bool> {
        match rt.is_running(inst).await {
            Ok(running) => Some(running),
            Err(e) => {
                tracing::warn!(instance = %inst.id, status = %inst.status, error = %e, pass, "runtime liveness unknown; leaving the row for the next pass");
                None
            }
        }
    }

    /// Bring a runtime object down by force: stop, and when that fails destroy
    /// the object (the disk stays). Returns a short description for the reason.
    async fn force_down(rt: &dyn Runtime, inst: &Instance) -> String {
        match rt.stop(inst, Duration::from_secs(STOP_GRACE_SECS)).await {
            Ok(()) => return "runtime stopped".to_string(),
            Err(e) if e.is_not_found() => return "runtime already gone".to_string(),
            Err(e) => {
                tracing::warn!(instance = %inst.id, error = %e, "stop failed; destroying the runtime object");
            }
        }
        match rt.destroy(inst, false).await {
            Ok(()) => "runtime destroyed".to_string(),
            Err(e) if e.is_not_found() => "runtime already gone".to_string(),
            Err(e) => format!("forced destroy failed: {}", describe(&e)),
        }
    }

    async fn reconcile_one(&self, inst: &mut Instance, now: i64) -> Result<()> {
        if inst.status == InstanceStatus::Deleted {
            return self.purge_records(inst).await;
        }
        let rt = match self.runtime_for(inst) {
            Ok(rt) => rt,
            Err(e) => {
                if inst.status.awake() || inst.status == InstanceStatus::Deleting {
                    tracing::warn!(instance = %inst.id, kind = %inst.kind, "runtime unavailable; marking failed");
                    inst.ip.clear();
                    return self
                        .transition(inst, InstanceStatus::Failed, &e.message)
                        .await;
                }
                return Ok(());
            }
        };
        let Some(runtime_running) = Self::liveness(rt.as_ref(), inst, "startup").await else {
            return Ok(());
        };
        let stop_leftover = |what: &'static str| {
            let rt = rt.clone();
            let inst = inst.clone();
            async move {
                if let Err(e) = rt.stop(&inst, Duration::from_secs(STOP_GRACE_SECS)).await
                    && !e.is_not_found()
                {
                    tracing::warn!(instance = %inst.id, error = %e, what, "could not stop leftover runtime");
                }
            }
        };
        match inst.status {
            InstanceStatus::Running => {
                if runtime_running {
                    self.touch_activity(&inst.id, now);
                    tracing::info!(instance = %inst.id, "adopted running instance");
                    Ok(())
                } else {
                    inst.stopped_at = now;
                    inst.ip.clear();
                    self.transition(
                        inst,
                        InstanceStatus::Stopped,
                        "runtime not running after restart",
                    )
                    .await
                }
            }
            InstanceStatus::Provisioning
            | InstanceStatus::Starting
            | InstanceStatus::Waking
            | InstanceStatus::Restarting
            | InstanceStatus::Updating => {
                if runtime_running {
                    stop_leftover("interrupted transition").await;
                }
                let reason = format!("daemon restarted during {}", inst.status);
                inst.ip.clear();
                self.transition(inst, InstanceStatus::Failed, &reason).await
            }
            InstanceStatus::Stopping => {
                if runtime_running {
                    stop_leftover("interrupted stop").await;
                }
                inst.stopped_at = now;
                inst.ip.clear();
                let reason = inst.status_reason.clone();
                self.transition(inst, InstanceStatus::Stopped, &reason)
                    .await
            }
            InstanceStatus::Deleting => self.delete_locked(inst).await,
            InstanceStatus::Deleted => self.purge_records(inst).await,
            InstanceStatus::Stopped | InstanceStatus::Sleeping | InstanceStatus::Failed => {
                if runtime_running {
                    tracing::warn!(instance = %inst.id, status = %inst.status, "runtime still running for a non-running instance; stopping it");
                    stop_leftover("non-running instance").await;
                }
                Ok(())
            }
        }
    }

    /// Periodic repair of rows the live operations cannot fix themselves:
    /// transitional rows not updated for [`STUCK_TRANSITION_SECS`] (the runtime
    /// is brought down and the row becomes `Failed`, or `Stopped` for an
    /// interrupted stop), `Failed` rows whose runtime still runs (brought down,
    /// storage kept), `Running` rows whose runtime is gone (`Stopped`), and
    /// `Deleting`/`Deleted` leftovers. Busy instances are skipped; a runtime
    /// that cannot answer leaves the row for the next pass.
    pub async fn repair(&self) {
        let all = match self.deps.store.list_all_instances().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "repair pass could not list instances");
                return;
            }
        };
        let now = self.now();
        for inst in all {
            let Some(_g) = self.try_lock(&inst.id) else {
                continue;
            };
            let mut fresh = match self.deps.store.get_instance(&inst.id).await {
                Ok(i) => i,
                Err(_) => continue,
            };
            if let Err(e) = self.repair_one(&mut fresh, now).await {
                tracing::error!(instance = %inst.id, status = %fresh.status, error = %e, "repair failed");
            }
        }
    }

    async fn repair_one(&self, inst: &mut Instance, now: i64) -> Result<()> {
        let stuck = now - inst.updated_at >= STUCK_TRANSITION_SECS;
        match inst.status {
            InstanceStatus::Deleted => return self.purge_records(inst).await,
            InstanceStatus::Deleting => {
                return if stuck {
                    tracing::warn!(instance = %inst.id, "finishing a stranded delete");
                    self.delete_locked(inst).await
                } else {
                    Ok(())
                };
            }
            InstanceStatus::Stopped | InstanceStatus::Sleeping => return Ok(()),
            _ => {}
        }
        let Ok(rt) = self.runtime_for(inst) else {
            return Ok(());
        };
        let Some(running) = Self::liveness(rt.as_ref(), inst, "repair").await else {
            return Ok(());
        };
        match inst.status {
            InstanceStatus::Running => {
                if running {
                    return Ok(());
                }
                tracing::warn!(instance = %inst.id, "runtime exited behind a running instance");
                inst.stopped_at = now;
                inst.ip.clear();
                self.transition(inst, InstanceStatus::Stopped, "runtime not running")
                    .await
            }
            InstanceStatus::Failed => {
                if !running {
                    return Ok(());
                }
                tracing::warn!(instance = %inst.id, "runtime still running behind a failed instance; bringing it down");
                let outcome = Self::force_down(rt.as_ref(), inst).await;
                inst.ip.clear();
                let reason = if inst.status_reason.is_empty() {
                    outcome
                } else {
                    format!("{}; {outcome}", inst.status_reason)
                };
                self.transition(inst, InstanceStatus::Failed, &reason).await
            }
            InstanceStatus::Provisioning
            | InstanceStatus::Starting
            | InstanceStatus::Waking
            | InstanceStatus::Restarting
            | InstanceStatus::Updating
            | InstanceStatus::Stopping => {
                if !stuck {
                    return Ok(());
                }
                let age = now - inst.updated_at;
                tracing::warn!(instance = %inst.id, status = %inst.status, age, "stranded transitional instance");
                let outcome = if running {
                    Self::force_down(rt.as_ref(), inst).await
                } else {
                    "runtime not running".to_string()
                };
                inst.ip.clear();
                if inst.status == InstanceStatus::Stopping {
                    inst.stopped_at = now;
                    let reason = if inst.status_reason.is_empty() {
                        format!("stop interrupted; {outcome}")
                    } else {
                        format!("{}; stop interrupted; {outcome}", inst.status_reason)
                    };
                    return self
                        .transition(inst, InstanceStatus::Stopped, &reason)
                        .await;
                }
                let reason = format!("stuck in {} for {age} seconds; {outcome}", inst.status);
                self.transition(inst, InstanceStatus::Failed, &reason).await
            }
            InstanceStatus::Stopped
            | InstanceStatus::Sleeping
            | InstanceStatus::Deleting
            | InstanceStatus::Deleted => Ok(()),
        }
    }

    /// Put to sleep every running instance with `auto_sleep` whose last
    /// activity is older than its idle timeout.
    pub async fn idle_sweep(&self) {
        let all = match self.deps.store.list_all_instances().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "idle sweep could not list instances");
                return;
            }
        };
        let now = self.now();
        for inst in all {
            if !inst.auto_sleep || inst.status != InstanceStatus::Running {
                continue;
            }
            let last = inst
                .last_activity_at
                .max(inst.started_at)
                .max(self.activity_of(&inst.id).unwrap_or(0));
            let idle_for = now - last;
            if idle_for <= i64::from(inst.idle_timeout_seconds) {
                continue;
            }
            // Busy instances are skipped and seen again on the next sweep.
            let Some(_g) = self.try_lock(&inst.id) else {
                continue;
            };
            let mut fresh = match self.deps.store.get_instance(&inst.id).await {
                Ok(i) => i,
                Err(_) => continue,
            };
            if fresh.status != InstanceStatus::Running || !fresh.auto_sleep {
                continue;
            }
            tracing::info!(instance = %inst.id, idle_for, timeout = inst.idle_timeout_seconds, "sleeping idle instance");
            if let Err(e) = self.sleep_locked(&mut fresh, "idle").await {
                tracing::error!(instance = %inst.id, error = %e, "idle sleep failed");
            }
        }
    }

    /// Record one metric point for every running instance.
    pub async fn sample_metrics(&self) {
        let all = match self.deps.store.list_all_instances().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "metrics sampler could not list instances");
                return;
            }
        };
        let now = self.now();
        for inst in all.iter().filter(|i| i.status == InstanceStatus::Running) {
            let Ok(rt) = self.runtime_for(inst) else {
                continue;
            };
            match rt.stats(inst).await {
                Ok(s) => {
                    let point = MetricPoint {
                        at: now,
                        cpu_percent: s.cpu_percent,
                        memory_bytes: s.memory_bytes,
                        disk_bytes: s.disk_bytes,
                    };
                    if let Err(e) = self.deps.store.insert_metric(&inst.id, point).await {
                        tracing::warn!(instance = %inst.id, error = %e, "could not store metric");
                    }
                }
                Err(e) => tracing::debug!(instance = %inst.id, error = %e, "stats unavailable"),
            }
        }
    }

    /// Drop metric points older than [`METRICS_RETENTION_SECS`].
    pub async fn prune_metrics(&self) {
        let before = self.now() - METRICS_RETENTION_SECS;
        if let Err(e) = self.deps.store.prune_metrics(before).await {
            tracing::warn!(error = %e, "could not prune metrics");
        }
    }

    /// Persist the in-memory activity map. Entries for busy instances or
    /// failed writes are kept for the next flush.
    pub async fn flush_activity(&self) {
        for (id, at) in self.drain_activity() {
            let Some(_g) = self.try_lock(&id) else {
                self.touch_activity(&id, at);
                continue;
            };
            match self.deps.store.get_instance(&id).await {
                Ok(mut inst) => {
                    if at > inst.last_activity_at {
                        inst.last_activity_at = at;
                        if let Err(e) = self.save(&mut inst).await {
                            tracing::warn!(instance = %id, error = %e, "could not flush activity");
                            self.touch_activity(&id, at);
                        }
                    }
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => {
                    tracing::warn!(instance = %id, error = %e, "could not load instance for activity flush");
                    self.touch_activity(&id, at);
                }
            }
        }
    }

    /// Ask every runtime how much disk each instance uses and store the answer.
    pub async fn refresh_disk_usage(&self) {
        let all = match self.deps.store.list_all_instances().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "disk refresh could not list instances");
                return;
            }
        };
        for inst in all {
            if matches!(
                inst.status,
                InstanceStatus::Provisioning | InstanceStatus::Deleting | InstanceStatus::Deleted
            ) || (inst.runtime_ref.is_empty() && inst.storage_path.is_empty())
            {
                continue;
            }
            let Ok(rt) = self.runtime_for(&inst) else {
                continue;
            };
            let bytes = match rt.disk_usage(&inst).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!(instance = %inst.id, error = %e, "disk usage unavailable");
                    continue;
                }
            };
            if bytes == inst.disk_used_bytes {
                continue;
            }
            let Some(_g) = self.try_lock(&inst.id) else {
                continue;
            };
            if let Ok(mut fresh) = self.deps.store.get_instance(&inst.id).await {
                fresh.disk_used_bytes = bytes;
                if let Err(e) = self.save(&mut fresh).await {
                    tracing::warn!(instance = %inst.id, error = %e, "could not store disk usage");
                }
            }
        }
    }
}
