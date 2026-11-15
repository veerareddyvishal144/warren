//! The [`Biller`]: prices shapes, enforces the wallet and awake-limit rules,
//! records top-ups and credits, and meters every instance on a timer.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use warren_core::{
    Biller as BillerApi, Clock, Config, Error, Instance, InstanceStopper, InstanceType,
    LedgerEntry, LedgerKind, Money, Rates, Result, Shape, Store, Workspace, dollars,
};

use crate::{limits, meter, meter::MeterStep};

/// `status_reason` the lifecycle records on instances the biller stops.
pub const STOP_REASON_INSUFFICIENT_BALANCE: &str = "insufficient_balance";

/// Meter period used when the configured `meter_interval` is zero, which
/// `tokio::time::interval` would refuse.
const FALLBACK_METER_INTERVAL: Duration = Duration::from_secs(60);

/// Prices, wallet rules and the per-minute meter. One per daemon, shared as an
/// `Arc` between the API, the lifecycle and the meter loop.
pub struct Biller {
    store: Arc<dyn Store>,
    cfg: Config,
    clock: Arc<dyn Clock>,
    stopper: RwLock<Option<Arc<dyn InstanceStopper>>>,
    /// Serializes ticks so two callers can never charge the same minutes twice.
    tick_lock: tokio::sync::Mutex<()>,
}

impl Biller {
    /// Build a biller over `store` that prices with `cfg.rates`, ticks every
    /// `cfg.meter_interval` and grants `cfg.free_credit_micros` at signup. Wire
    /// an [`InstanceStopper`] with [`Biller::set_stopper`] before [`Biller::run`]
    /// so exhausted wallets can stop their instances.
    pub fn new(store: Arc<dyn Store>, cfg: Config, clock: Arc<dyn Clock>) -> Arc<Biller> {
        Arc::new(Biller {
            store,
            cfg,
            clock,
            stopper: RwLock::new(None),
            tick_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Wire the component that stops instances when a wallet is exhausted.
    /// Until one is set, exhausted wallets are logged and nothing is stopped.
    pub fn set_stopper(&self, s: Arc<dyn InstanceStopper>) {
        *self.stopper.write() = Some(s);
    }

    /// Tick on `Config::meter_interval` until `shutdown` fires. The first tick
    /// runs immediately, so minutes accrued while the daemon was down are
    /// settled at startup. A tick in progress always completes before the loop
    /// observes the cancellation. Tick errors are logged and the loop continues.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> Result<()> {
        let period = if self.cfg.meter_interval.is_zero() {
            warn!(fallback = ?FALLBACK_METER_INTERVAL, "meter_interval is zero; using the fallback period");
            FALLBACK_METER_INTERVAL
        } else {
            self.cfg.meter_interval
        };
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        info!(period = ?period, "billing meter started");
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("billing meter stopped");
                    return Ok(());
                }
                _ = interval.tick() => {
                    let now = self.clock.now_unix();
                    if let Err(e) = BillerApi::tick(self.as_ref(), now).await {
                        error!(error = %e, "billing tick failed");
                    }
                }
            }
        }
    }

    /// Record a top-up of `micros` (5 USD to 10,000 USD inclusive, else
    /// `invalid_request`) and return the refreshed workspace with the ledger
    /// entry. Top-ups raise `Workspace::top_up_total_micros` and so climb the
    /// awake ladder. The trait's `top_up` is this without the entry.
    pub async fn apply_top_up(
        &self,
        workspace_id: &str,
        micros: Money,
        note: &str,
    ) -> Result<(Workspace, LedgerEntry)> {
        if !limits::top_up_in_bounds(micros) {
            return Err(Error::invalid(format!(
                "top-up amount must be between {} and {} micros ({:.2} and {:.2} USD); got {micros}",
                limits::MIN_TOP_UP_MICROS,
                limits::MAX_TOP_UP_MICROS,
                dollars(limits::MIN_TOP_UP_MICROS),
                dollars(limits::MAX_TOP_UP_MICROS)
            )));
        }
        self.record(workspace_id, LedgerKind::Topup, micros, note)
            .await
    }

    /// Record a credit of any positive amount (else `invalid_request`) and
    /// return the refreshed workspace with the ledger entry. Credits move the
    /// wallet but not the top-up total.
    pub async fn apply_credit(
        &self,
        workspace_id: &str,
        micros: Money,
        note: &str,
    ) -> Result<(Workspace, LedgerEntry)> {
        if micros <= 0 {
            return Err(Error::invalid(format!(
                "credit amount must be positive; got {micros}"
            )));
        }
        self.record(workspace_id, LedgerKind::Credit, micros, note)
            .await
    }

    /// Grant `Config::free_credit_micros` once and mark the workspace. Calling
    /// it again returns the workspace unchanged. Meant for signup, before the
    /// workspace has instances: it rewrites the workspace row to set the flag.
    pub async fn grant_free_credit(&self, workspace_id: &str) -> Result<Workspace> {
        let mut ws = self.store.get_workspace(workspace_id).await?;
        if ws.free_credit_granted {
            return Ok(ws);
        }
        if self.cfg.free_credit_micros > 0 {
            ws = self
                .record(
                    workspace_id,
                    LedgerKind::Credit,
                    self.cfg.free_credit_micros,
                    "free credit",
                )
                .await?
                .0;
        }
        ws.free_credit_granted = true;
        self.store.update_workspace(&ws).await?;
        Ok(ws)
    }

    /// Write a workspace-level ledger entry and return the refreshed workspace.
    async fn record(
        &self,
        workspace_id: &str,
        kind: LedgerKind,
        micros: Money,
        note: &str,
    ) -> Result<(Workspace, LedgerEntry)> {
        // Fail with not_found before writing anything for an unknown workspace.
        self.store.get_workspace(workspace_id).await?;
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
        let ws = self.store.get_workspace(workspace_id).await?;
        info!(workspace = %workspace_id, kind = kind.as_str(), micros, wallet = ws.wallet_micros, "ledger entry recorded");
        Ok((ws, entry))
    }

    /// The awake-limit half of the create and wake checks.
    async fn assert_awake_room(&self, ws: &Workspace) -> Result<()> {
        let limit = limits::awake_limit(ws.top_up_total_micros);
        let awake = self.store.count_awake_instances(&ws.id).await?;
        if awake >= limit {
            return Err(Error::instance_limit(limit, awake));
        }
        Ok(())
    }

    /// Charge one instance for the whole minutes since the meter position the
    /// listing saw and move the meter. The meter is written first, as a
    /// compare-and-swap on that position: when the lifecycle moved it since the
    /// listing (start and wake reset it) this tick skips the instance instead of
    /// rewinding the meter or charging the same minutes twice, and the next tick
    /// meters from the new position. The ledger entry follows the meter write, so
    /// a charge is never written for minutes the meter did not consume; if the
    /// ledger write fails the meter is moved back so the retry charges the span
    /// exactly once.
    async fn meter_instance(&self, inst: &Instance, now: i64) -> Result<()> {
        let Some(from) = meter::metered_from(inst) else {
            return Ok(());
        };
        let Some((kind, monthly)) = meter::billing_rate(&self.cfg.rates, inst) else {
            return Ok(());
        };
        let Some(step) = meter::advance(monthly, inst.meter_carry_micros, from, now) else {
            return Ok(());
        };

        let moved = match self
            .store
            .set_meter(
                &inst.id,
                step.metered_through,
                step.carry_micros,
                Some(inst.metered_through),
            )
            .await
        {
            Ok(moved) => moved,
            Err(e) if e.is_not_found() => {
                debug!(instance = %inst.id, "instance disappeared while metering; nothing charged");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if !moved {
            debug!(
                instance = %inst.id,
                workspace = %inst.workspace_id,
                listed_through = inst.metered_through,
                "meter moved by the lifecycle since the listing; skipping this tick"
            );
            return Ok(());
        }
        if step.charge_micros <= 0 {
            return Ok(());
        }

        let mut entry = LedgerEntry {
            id: String::new(),
            workspace_id: inst.workspace_id.clone(),
            instance_id: inst.id.clone(),
            kind,
            micros: -step.charge_micros,
            minutes: step.minutes as f64,
            at: now,
            note: String::new(),
        };
        if let Err(e) = self.store.apply_ledger(&mut entry).await {
            self.rewind_meter(inst, &step, kind, &e).await;
            return Err(e);
        }
        debug!(
            instance = %inst.id,
            workspace = %inst.workspace_id,
            kind = kind.as_str(),
            status = %inst.status,
            minutes = step.minutes,
            micros = step.charge_micros,
            carry = step.carry_micros,
            "metered"
        );
        Ok(())
    }

    /// Best-effort compensation when the ledger entry could not be written after
    /// the meter had moved: put the meter back where the listing saw it, again
    /// as a compare-and-swap so a lifecycle write in between stands, and let the
    /// next tick charge the whole span once. When that fails too the span stays
    /// unbilled, which is logged; the wallet is never over-charged.
    async fn rewind_meter(
        &self,
        inst: &Instance,
        step: &MeterStep,
        kind: LedgerKind,
        cause: &Error,
    ) {
        match self
            .store
            .set_meter(
                &inst.id,
                inst.metered_through,
                inst.meter_carry_micros,
                Some(step.metered_through),
            )
            .await
        {
            Ok(true) => warn!(
                instance = %inst.id,
                kind = kind.as_str(),
                micros = step.charge_micros,
                error = %cause,
                "charge not written; meter moved back for the next tick"
            ),
            Ok(false) => warn!(
                instance = %inst.id,
                kind = kind.as_str(),
                micros = step.charge_micros,
                error = %cause,
                "charge not written and the lifecycle moved the meter meanwhile; the span stays unbilled"
            ),
            Err(e) => error!(
                instance = %inst.id,
                kind = kind.as_str(),
                micros = step.charge_micros,
                error = %cause,
                rollback_error = %e,
                "charge not written and the meter could not be moved back; the span stays unbilled"
            ),
        }
    }

    /// Stop every compute-billing instance of each listed workspace whose wallet
    /// is at or below zero. Failures are logged; nothing is returned.
    async fn stop_exhausted(&self, workspace_ids: BTreeSet<String>) {
        if workspace_ids.is_empty() {
            return;
        }
        let stopper = self.stopper.read().clone();
        for ws_id in workspace_ids {
            let ws = match self.store.get_workspace(&ws_id).await {
                Ok(ws) => ws,
                Err(e) => {
                    warn!(workspace = %ws_id, error = %e, "cannot read workspace after metering");
                    continue;
                }
            };
            if ws.wallet_micros > 0 {
                continue;
            }
            let Some(stopper) = &stopper else {
                warn!(workspace = %ws_id, wallet = ws.wallet_micros, "wallet exhausted but no stopper is wired; instances keep running");
                continue;
            };
            let instances = match self.store.list_instances(&ws_id).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(workspace = %ws_id, error = %e, "cannot list instances of an exhausted workspace");
                    continue;
                }
            };
            for inst in instances.iter().filter(|i| i.status.bills_compute()) {
                info!(instance = %inst.id, workspace = %ws_id, wallet = ws.wallet_micros, status = %inst.status, "stopping: wallet exhausted");
                if let Err(e) = stopper
                    .stop_for_billing(&inst.id, STOP_REASON_INSUFFICIENT_BALANCE)
                    .await
                {
                    error!(instance = %inst.id, workspace = %ws_id, error = %e, "could not stop instance for billing");
                }
            }
        }
    }
}

#[async_trait]
impl BillerApi for Biller {
    fn rates(&self) -> Rates {
        self.cfg.rates
    }

    fn monthly_running_micros(&self, shape: Shape, t: InstanceType) -> Money {
        self.cfg.rates.monthly_running(shape, t)
    }

    fn monthly_disk_micros(&self, shape: Shape) -> Money {
        self.cfg.rates.monthly_disk(shape)
    }

    async fn assert_can_create(&self, ws: &Workspace, shape: Shape, t: InstanceType) -> Result<()> {
        let required = limits::required_balance_to_create(self.cfg.rates.monthly_running(shape, t));
        if ws.wallet_micros < required {
            return Err(Error::insufficient_balance(required, ws.wallet_micros));
        }
        self.assert_awake_room(ws).await
    }

    async fn assert_can_wake(&self, ws: &Workspace) -> Result<()> {
        if ws.wallet_micros <= 0 {
            return Err(Error::insufficient_balance(1, ws.wallet_micros));
        }
        self.assert_awake_room(ws).await
    }

    fn awake_limit(&self, ws: &Workspace) -> u32 {
        limits::awake_limit(ws.top_up_total_micros)
    }

    async fn top_up(&self, workspace_id: &str, micros: Money, note: &str) -> Result<Workspace> {
        Ok(self.apply_top_up(workspace_id, micros, note).await?.0)
    }

    async fn grant_credit(
        &self,
        workspace_id: &str,
        micros: Money,
        note: &str,
    ) -> Result<Workspace> {
        Ok(self.apply_credit(workspace_id, micros, note).await?.0)
    }

    async fn tick(&self, now: i64) -> Result<()> {
        let _serialized = self.tick_lock.lock().await;
        let instances = self.store.list_all_instances().await?;
        let mut compute_workspaces = BTreeSet::new();
        let mut failures = 0usize;
        for inst in &instances {
            if inst.status.bills_compute() {
                compute_workspaces.insert(inst.workspace_id.clone());
            }
            if let Err(e) = self.meter_instance(inst, now).await {
                failures += 1;
                error!(instance = %inst.id, workspace = %inst.workspace_id, error = %e, "metering failed; will retry next tick");
            }
        }
        self.stop_exhausted(compute_workspaces).await;
        if failures > 0 {
            return Err(Error::internal(format!(
                "{failures} of {} instances could not be metered",
                instances.len()
            )));
        }
        Ok(())
    }
}
