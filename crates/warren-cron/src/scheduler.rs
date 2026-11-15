//! The polling loop: list due crons, fire them with bounded concurrency and
//! never fire the same cron twice at once.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::Semaphore,
    task::{Id, JoinError, JoinSet},
    time::MissedTickBehavior,
};
use tokio_util::sync::CancellationToken;
use warren_core::{Clock, Config, Error, GATEWAY_PORT, Lifecycle, Result, Store};

use crate::firing::Firer;

/// How often due crons are looked up.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How long one firing may take, from connecting to the gateway until the whole
/// response has been read.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Firings that may run at the same time.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// How long in-flight firings may finish after shutdown is requested before
/// they are aborted.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Firings currently running, indexed by task and by cron id.
#[derive(Default)]
struct InFlight {
    by_task: HashMap<Id, String>,
    cron_ids: HashSet<String>,
}

impl InFlight {
    fn insert(&mut self, task: Id, cron_id: String) {
        self.cron_ids.insert(cron_id.clone());
        self.by_task.insert(task, cron_id);
    }
    fn remove(&mut self, task: &Id) -> Option<String> {
        let cron_id = self.by_task.remove(task)?;
        self.cron_ids.remove(&cron_id);
        Some(cron_id)
    }
    fn contains(&self, cron_id: &str) -> bool {
        self.cron_ids.contains(cron_id)
    }
    fn len(&self) -> usize {
        self.by_task.len()
    }
}

/// Drives scheduled prompts. Build with [`Scheduler::new`], optionally tune
/// with the `with_*` methods, then [`Scheduler::run`] until the token fires.
pub struct Scheduler {
    store: Arc<dyn Store>,
    lifecycle: Arc<dyn Lifecycle>,
    cfg: Config,
    clock: Arc<dyn Clock>,
    poll_interval: Duration,
    request_timeout: Duration,
    max_concurrent: usize,
    gateway_port: u16,
    shutdown_grace: Duration,
}

impl Scheduler {
    /// A scheduler with the production defaults: poll every 30 s, 10 minute
    /// request timeout, four concurrent firings, gateway port 3737.
    pub fn new(
        store: Arc<dyn Store>,
        lifecycle: Arc<dyn Lifecycle>,
        cfg: Config,
        clock: Arc<dyn Clock>,
    ) -> Scheduler {
        Scheduler {
            store,
            lifecycle,
            cfg,
            clock,
            poll_interval: DEFAULT_POLL_INTERVAL,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            gateway_port: GATEWAY_PORT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Poll period (at least 1 ms). The first poll happens as soon as `run` starts.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_millis(1));
        self
    }

    /// Whole-request timeout for one `POST /v1/responses`.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Maximum number of firings in flight at once (at least 1).
    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }

    /// Port the gateway listens on inside instances. Tests point this at a
    /// local stand-in server.
    pub fn with_gateway_port(mut self, port: u16) -> Self {
        self.gateway_port = port;
        self
    }

    /// How long in-flight firings may finish after shutdown before being aborted.
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// Poll and fire until `shutdown` is cancelled, then give in-flight firings
    /// the grace period and return. Fails only when the HTTP client cannot be
    /// built.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(self.request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(format!("warren-cron/{}", self.cfg.version))
            .no_proxy()
            .build()
            .map_err(|e| Error::internal(format!("cron: building the HTTP client: {e}")))?;
        let firer = Arc::new(Firer {
            store: self.store.clone(),
            lifecycle: self.lifecycle.clone(),
            clock: self.clock.clone(),
            client,
            gateway_port: self.gateway_port,
            request_timeout: self.request_timeout,
        });
        let limiter = Arc::new(Semaphore::new(self.max_concurrent));
        let mut in_flight = InFlight::default();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tracing::info!(
            poll_secs = self.poll_interval.as_secs_f64(),
            max_concurrent = self.max_concurrent,
            request_timeout_secs = self.request_timeout.as_secs(),
            "cron scheduler running"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => self.poll(&firer, &limiter, &mut in_flight, &mut tasks).await,
                Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => Self::finish(done, &mut in_flight),
            }
        }
        self.drain(&mut tasks).await;
        tracing::info!("cron scheduler stopped");
        Ok(())
    }

    /// One poll: list due crons and start a firing task for each one that is
    /// enabled and not already in flight.
    async fn poll(
        &self,
        firer: &Arc<Firer>,
        limiter: &Arc<Semaphore>,
        in_flight: &mut InFlight,
        tasks: &mut JoinSet<()>,
    ) {
        let now = self.clock.now_unix();
        let due = match self.store.list_due_crons(now).await {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!(error = %e, details = ?e.details, "cron: listing due crons failed; retrying at the next poll");
                return;
            }
        };
        if !due.is_empty() {
            tracing::debug!(due = due.len(), in_flight = in_flight.len(), "cron poll");
        }
        for cron in due {
            if !cron.enabled {
                tracing::debug!(cron_id = %cron.id, "cron: store listed a disabled cron as due; skipping");
                continue;
            }
            if in_flight.contains(&cron.id) {
                tracing::debug!(cron_id = %cron.id, "cron: still firing from an earlier poll; skipping");
                continue;
            }
            if cron.next_run_at == 0 {
                // Never scheduled (created without a next run): compute the first
                // run instead of firing at whatever moment the poll happened.
                let mut cron = cron;
                tracing::info!(cron_id = %cron.id, schedule = %cron.schedule, "cron: computing first run");
                firer.advance(&mut cron, now);
                firer.save(&cron).await;
                continue;
            }
            let cron_id = cron.id.clone();
            let (firer, limiter) = (firer.clone(), limiter.clone());
            let handle = tasks.spawn(async move {
                // The semaphore is never closed, so this only fails if the
                // scheduler is gone, in which case there is nothing to do.
                let Ok(_permit) = limiter.acquire_owned().await else {
                    return;
                };
                firer.fire(cron).await;
            });
            in_flight.insert(handle.id(), cron_id);
        }
    }

    /// Forget a finished firing so the cron may be picked up again.
    fn finish(done: std::result::Result<(Id, ()), JoinError>, in_flight: &mut InFlight) {
        match done {
            Ok((id, ())) => {
                in_flight.remove(&id);
            }
            Err(e) => {
                let cron_id = in_flight.remove(&e.id()).unwrap_or_default();
                if e.is_panic() {
                    tracing::error!(cron_id = %cron_id, error = %e, "cron: firing task panicked; the cron stays due");
                } else {
                    tracing::debug!(cron_id = %cron_id, "cron: firing task aborted");
                }
            }
        }
    }

    /// Let in-flight firings finish within the grace period, then abort the rest.
    async fn drain(&self, tasks: &mut JoinSet<()>) {
        if tasks.is_empty() {
            return;
        }
        tracing::info!(
            in_flight = tasks.len(),
            grace_secs = self.shutdown_grace.as_secs_f64(),
            "cron: waiting for in-flight firings"
        );
        let deadline = tokio::time::sleep(self.shutdown_grace);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                next = tasks.join_next() => if next.is_none() { break },
            }
        }
        if !tasks.is_empty() {
            tracing::warn!(
                aborted = tasks.len(),
                "cron: aborting firings that did not finish in time; they stay due and fire again after restart"
            );
            tasks.shutdown().await;
        }
    }
}
