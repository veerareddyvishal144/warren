//! Loop tests: fake store and lifecycle, a local HTTP server as the gateway.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use warren_core::{Config, Cron, Error};

use crate::{
    STATUS_INVALID_SCHEDULE, STATUS_OK, Scheduler, schedule,
    testing::{FakeGateway, FakeLifecycle, FakeStore, FixedClock, Reply},
};

/// 2026-01-01T00:00:00Z.
const NOW: i64 = 1_767_225_600;
const INSTANCE: &str = "ab12cd34ef";
const POLL: Duration = Duration::from_millis(20);
const WAIT: Duration = Duration::from_secs(10);

fn cron(id: &str, schedule: &str) -> Cron {
    Cron {
        id: id.to_string(),
        instance_id: INSTANCE.to_string(),
        workspace_id: "ws_test".to_string(),
        name: id.to_string(),
        prompt: format!("run {id}"),
        schedule: schedule.to_string(),
        timezone: String::new(),
        enabled: true,
        next_run_at: NOW - 1,
        created_at: NOW - 100,
        updated_at: NOW - 100,
        ..Default::default()
    }
}

async fn wait_for(cond: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while !cond() {
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

struct Harness {
    store: Arc<FakeStore>,
    lifecycle: Arc<FakeLifecycle>,
    gateway: FakeGateway,
    clock: Arc<FixedClock>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<warren_core::Result<()>>,
}

impl Harness {
    /// Scheduler polling every 20 ms against a fake gateway answering `reply`.
    async fn start(crons: Vec<Cron>, reply: Reply) -> Harness {
        let gateway = FakeGateway::spawn(reply).await;
        Self::start_with(crons, gateway, FakeLifecycle::at("127.0.0.1"), |s| s).await
    }

    async fn start_with(
        crons: Vec<Cron>,
        gateway: FakeGateway,
        lifecycle: Arc<FakeLifecycle>,
        tune: impl FnOnce(Scheduler) -> Scheduler,
    ) -> Harness {
        let store = FakeStore::with_crons(crons);
        let clock = FixedClock::at(NOW);
        let scheduler = Scheduler::new(
            store.clone(),
            lifecycle.clone(),
            Config::default(),
            clock.clone(),
        )
        .with_poll_interval(POLL)
        .with_gateway_port(gateway.port())
        .with_shutdown_grace(Duration::from_millis(200));
        let scheduler = tune(scheduler);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(scheduler.run(shutdown.clone()));
        Harness {
            store,
            lifecycle,
            gateway,
            clock,
            shutdown,
            task,
        }
    }

    async fn wait_updates(&self, n: usize) {
        assert!(
            wait_for(|| self.store.update_count() >= n, WAIT).await,
            "expected {n} cron updates, saw {}",
            self.store.update_count()
        );
    }

    async fn wait_requests(&self, n: usize) {
        assert!(
            wait_for(|| self.gateway.requests().len() >= n, WAIT).await,
            "expected {n} requests, saw {}",
            self.gateway.requests().len()
        );
    }

    /// Cancel and require `run` to return promptly.
    async fn stop(&mut self) -> warren_core::Result<()> {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .expect("run() must return soon after cancellation")
            .expect("run() must not panic")
    }
}

#[tokio::test]
async fn fires_a_due_cron_and_records_ok() {
    let mut h = Harness::start(vec![cron("c1", "*/15 * * * *")], Reply::completed()).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();

    let requests = h.gateway.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/responses");
    assert!(
        requests[0].content_type.starts_with("application/json"),
        "{}",
        requests[0].content_type
    );
    assert_eq!(
        requests[0].body,
        serde_json::json!({ "input": "run c1", "metadata": { "cron_id": "c1" } })
    );

    let updated = h.store.cron("c1");
    assert_eq!(updated.last_status, STATUS_OK);
    assert_eq!(updated.last_run_at, NOW);
    assert_eq!(
        updated.next_run_at,
        schedule::next_run("*/15 * * * *", "", NOW).unwrap()
    );
    assert!(updated.next_run_at > NOW);
    assert!(updated.enabled);
    assert_eq!(updated.updated_at, NOW);
    assert_eq!(h.lifecycle.awake_calls(), vec![INSTANCE.to_string()]);
    assert!(h.lifecycle.activity.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn honours_the_cron_timezone_for_the_next_run() {
    let mut c = cron("tz", "0 9 * * *");
    c.timezone = "America/New_York".to_string();
    let mut h = Harness::start(vec![c], Reply::completed()).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    // 09:00 EST on 2026-01-01 is 14:00Z.
    assert_eq!(h.store.cron("tz").next_run_at, NOW + 14 * 3600);
}

#[tokio::test]
async fn wake_failure_is_recorded_without_posting() {
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let lifecycle =
        FakeLifecycle::failing_wake("127.0.0.1", Error::conflict("instance is stopped"));
    let mut h = Harness::start_with(vec![cron("c1", "0 * * * *")], gateway, lifecycle, |s| s).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();

    assert!(h.gateway.requests().is_empty());
    let updated = h.store.cron("c1");
    assert_eq!(
        updated.last_status,
        "wake failed: conflict: instance is stopped"
    );
    assert_eq!(updated.last_run_at, NOW);
    assert_eq!(updated.next_run_at, NOW + 3600);
    assert!(updated.enabled);
}

#[tokio::test]
async fn endpoint_failure_is_an_error_status() {
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let lifecycle = FakeLifecycle::failing_endpoint(Error::not_found("instance"));
    let mut h = Harness::start_with(vec![cron("c1", "0 * * * *")], gateway, lifecycle, |s| s).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert!(h.gateway.requests().is_empty());
    assert_eq!(
        h.store.cron("c1").last_status,
        "error: not_found: instance not found"
    );
}

#[tokio::test]
async fn gateway_error_status_is_recorded() {
    let mut h = Harness::start(
        vec![cron("c1", "0 * * * *")],
        Reply::error(500, "internal", "boom"),
    )
    .await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    let updated = h.store.cron("c1");
    assert_eq!(updated.last_status, "error: HTTP 500 boom");
    assert_eq!(updated.next_run_at, NOW + 3600);
    assert!(updated.enabled);
}

#[tokio::test]
async fn failed_agent_response_is_an_error() {
    let mut h = Harness::start(
        vec![cron("c1", "0 * * * *")],
        Reply::failed("model unavailable"),
    )
    .await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert_eq!(h.store.cron("c1").last_status, "error: model unavailable");
}

#[tokio::test]
async fn connection_refused_is_an_error() {
    // A port nothing listens on: bind, learn the port, close it.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let lifecycle = FakeLifecycle::at(format!("127.0.0.1:{closed}"));
    let mut h = Harness::start_with(vec![cron("c1", "0 * * * *")], gateway, lifecycle, |s| s).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    let status = h.store.cron("c1").last_status;
    assert!(status.starts_with("error: "), "{status}");
    assert!(h.gateway.requests().is_empty());
    assert_eq!(h.store.cron("c1").next_run_at, NOW + 3600);
}

#[tokio::test]
async fn slow_gateway_times_out() {
    let gateway = FakeGateway::spawn(Reply::completed().delayed(Duration::from_secs(30))).await;
    let mut h = Harness::start_with(
        vec![cron("c1", "0 * * * *")],
        gateway,
        FakeLifecycle::at("127.0.0.1"),
        |s| s.with_request_timeout(Duration::from_millis(200)),
    )
    .await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert_eq!(
        h.store.cron("c1").last_status,
        "error: no response within 0s"
    );
}

#[tokio::test]
async fn invalid_schedule_disables_the_cron_after_firing() {
    let mut bad_tz = cron("tz", "0 9 * * *");
    bad_tz.timezone = "Mars/Phobos".to_string();
    let mut h = Harness::start(
        vec![cron("c1", "every day at nine"), bad_tz],
        Reply::completed(),
    )
    .await;
    h.wait_updates(2).await;
    h.stop().await.unwrap();

    // The prompt was still delivered once; the cron is then switched off.
    assert_eq!(h.gateway.requests().len(), 2);
    for id in ["c1", "tz"] {
        let updated = h.store.cron(id);
        assert!(!updated.enabled, "{id} should be disabled");
        assert_eq!(updated.last_status, STATUS_INVALID_SCHEDULE);
        assert_eq!(updated.last_run_at, NOW);
    }
}

#[tokio::test]
async fn never_scheduled_cron_gets_a_first_run_instead_of_firing() {
    let mut fresh = cron("fresh", "0 9 * * *");
    fresh.next_run_at = 0;
    let mut h = Harness::start(vec![fresh], Reply::completed()).await;
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert!(h.gateway.requests().is_empty());
    let updated = h.store.cron("fresh");
    assert_eq!(updated.next_run_at, NOW + 9 * 3600);
    assert_eq!(updated.last_run_at, 0);
    assert_eq!(updated.last_status, "");
    assert!(updated.enabled);
    assert!(h.lifecycle.awake_calls().is_empty());
}

#[tokio::test]
async fn fires_again_when_the_clock_reaches_the_next_run() {
    let mut h = Harness::start(vec![cron("hourly", "0 * * * *")], Reply::completed()).await;
    h.wait_updates(1).await;
    let first = h.store.cron("hourly");
    assert_eq!(first.last_run_at, NOW);
    assert_eq!(first.next_run_at, NOW + 3600);

    // Not due yet: a few more polls must not fire it.
    h.clock.set(NOW + 3599);
    assert!(wait_for(|| h.store.list_calls.load(Ordering::SeqCst) >= 6, WAIT).await);
    assert_eq!(h.gateway.requests().len(), 1);

    // Due again once the clock reaches the scheduled second.
    h.clock.set(NOW + 3600);
    h.wait_updates(2).await;
    h.stop().await.unwrap();
    assert_eq!(h.gateway.requests().len(), 2);
    let second = h.store.cron("hourly");
    assert_eq!(second.last_run_at, NOW + 3600);
    assert_eq!(second.next_run_at, NOW + 7200);
    assert_eq!(second.last_status, STATUS_OK);
}

#[tokio::test]
async fn runs_at_most_four_firings_at_once() {
    let crons: Vec<Cron> = (0..8)
        .map(|i| cron(&format!("c{i}"), "0 * * * *"))
        .collect();
    let mut h = Harness::start(
        crons,
        Reply::completed().delayed(Duration::from_millis(400)),
    )
    .await;
    h.wait_updates(8).await;
    h.stop().await.unwrap();
    assert_eq!(h.gateway.requests().len(), 8);
    assert_eq!(h.gateway.max_concurrent(), 4);
    for i in 0..8 {
        assert_eq!(h.store.cron(&format!("c{i}")).last_status, STATUS_OK);
    }
}

#[tokio::test]
async fn concurrency_limit_is_configurable() {
    let crons: Vec<Cron> = (0..5)
        .map(|i| cron(&format!("c{i}"), "0 * * * *"))
        .collect();
    let gateway = FakeGateway::spawn(Reply::completed().delayed(Duration::from_millis(300))).await;
    let mut h = Harness::start_with(crons, gateway, FakeLifecycle::at("127.0.0.1"), |s| {
        s.with_max_concurrent(2)
    })
    .await;
    h.wait_updates(5).await;
    h.stop().await.unwrap();
    assert_eq!(h.gateway.max_concurrent(), 2);
}

#[tokio::test]
async fn edits_made_during_a_firing_are_kept() {
    let mut h = Harness::start(
        vec![cron("c1", "0 * * * *")],
        Reply::completed().delayed(Duration::from_millis(300)),
    )
    .await;
    h.wait_requests(1).await;
    {
        let mut crons = h.store.crons.lock().unwrap();
        let c = crons.iter_mut().find(|c| c.id == "c1").unwrap();
        c.enabled = false;
        c.name = "renamed".to_string();
        c.prompt = "new prompt".to_string();
    }
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    let updated = h.store.cron("c1");
    assert!(
        !updated.enabled,
        "a disable during the firing must not be undone"
    );
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.prompt, "new prompt");
    assert_eq!(updated.last_status, STATUS_OK);
    assert_eq!(updated.last_run_at, NOW);
    assert_eq!(updated.next_run_at, NOW + 3600);
}

#[tokio::test]
async fn a_cron_deleted_during_a_firing_is_not_written_back() {
    let mut h = Harness::start(
        vec![cron("gone", "0 * * * *")],
        Reply::completed().delayed(Duration::from_millis(300)),
    )
    .await;
    h.wait_requests(1).await;
    h.store.crons.lock().unwrap().clear();
    // Let the firing complete and a few more polls run.
    let polls = h.store.list_calls.load(Ordering::SeqCst);
    assert!(
        wait_for(
            || h.store.list_calls.load(Ordering::SeqCst) >= polls + 25,
            WAIT
        )
        .await
    );
    h.stop().await.unwrap();
    assert_eq!(h.gateway.requests().len(), 1);
    assert_eq!(h.store.update_calls.load(Ordering::SeqCst), 0);
    assert!(h.store.crons.lock().unwrap().is_empty());
}

#[tokio::test]
async fn transient_save_failures_are_retried_without_refiring() {
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let mut h = Harness::start_with(
        vec![cron("c1", "0 * * * *")],
        gateway,
        FakeLifecycle::at("127.0.0.1"),
        |s| s,
    )
    .await;
    h.store.update_failures.store(1, Ordering::SeqCst);
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert_eq!(h.store.update_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.gateway.requests().len(),
        1,
        "a store hiccup must not re-post the prompt"
    );
    assert_eq!(h.store.cron("c1").last_status, STATUS_OK);
}

#[tokio::test]
async fn an_in_flight_cron_is_not_fired_again_by_later_polls() {
    let mut h = Harness::start(
        vec![cron("slow", "0 * * * *")],
        Reply::completed().delayed(Duration::from_millis(300)),
    )
    .await;
    h.wait_updates(1).await;
    // Give a few more polls the chance to misbehave after completion too.
    tokio::time::sleep(POLL * 5).await;
    h.stop().await.unwrap();
    assert!(
        h.store.list_calls.load(Ordering::SeqCst) >= 5,
        "the loop should have polled repeatedly while the firing was in flight"
    );
    assert_eq!(h.gateway.requests().len(), 1);
    assert_eq!(h.store.update_count(), 1);
}

#[tokio::test]
async fn store_failures_are_retried_on_the_next_poll() {
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let mut h = Harness::start_with(
        vec![cron("c1", "0 * * * *")],
        gateway,
        FakeLifecycle::at("127.0.0.1"),
        |s| s,
    )
    .await;
    // The harness starts the loop immediately; make the next two listings fail.
    h.store.list_failures.store(2, Ordering::SeqCst);
    h.wait_updates(1).await;
    h.stop().await.unwrap();
    assert_eq!(h.store.cron("c1").last_status, STATUS_OK);
}

#[tokio::test]
async fn disabled_crons_listed_by_the_store_are_skipped() {
    let mut off = cron("off", "0 * * * *");
    off.enabled = false;
    let gateway = FakeGateway::spawn(Reply::completed()).await;
    let mut h =
        Harness::start_with(vec![off], gateway, FakeLifecycle::at("127.0.0.1"), |s| s).await;
    h.store.include_disabled.store(true, Ordering::SeqCst);
    assert!(wait_for(|| h.store.list_calls.load(Ordering::SeqCst) >= 3, WAIT).await);
    h.stop().await.unwrap();
    assert!(h.gateway.requests().is_empty());
    assert_eq!(h.store.update_count(), 0);
}

#[tokio::test]
async fn nothing_due_means_no_calls() {
    let mut later = cron("later", "0 * * * *");
    later.next_run_at = NOW + 60;
    let mut h = Harness::start(vec![later], Reply::completed()).await;
    assert!(wait_for(|| h.store.list_calls.load(Ordering::SeqCst) >= 3, WAIT).await);
    h.stop().await.unwrap();
    assert!(h.gateway.requests().is_empty());
    assert!(h.lifecycle.awake_calls().is_empty());
    assert_eq!(h.store.update_count(), 0);
}

#[tokio::test]
async fn shutdown_aborts_firings_after_the_grace_period() {
    let gateway = FakeGateway::spawn(Reply::completed().delayed(Duration::from_secs(30))).await;
    let mut h = Harness::start_with(
        vec![cron("stuck", "0 * * * *")],
        gateway,
        FakeLifecycle::at("127.0.0.1"),
        |s| s.with_shutdown_grace(Duration::from_millis(100)),
    )
    .await;
    h.wait_requests(1).await;
    let started = Instant::now();
    h.stop().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stop took {:?}",
        started.elapsed()
    );
    // Nothing was recorded: the cron stays due and fires again after a restart.
    assert_eq!(h.store.update_count(), 0);
    assert_eq!(h.store.cron("stuck").next_run_at, NOW - 1);
}

#[tokio::test]
async fn shutdown_lets_quick_firings_finish() {
    let gateway = FakeGateway::spawn(Reply::completed().delayed(Duration::from_millis(100))).await;
    let mut h = Harness::start_with(
        vec![cron("quick", "0 * * * *")],
        gateway,
        FakeLifecycle::at("127.0.0.1"),
        |s| s.with_shutdown_grace(Duration::from_secs(5)),
    )
    .await;
    h.wait_requests(1).await;
    h.stop().await.unwrap();
    assert_eq!(h.store.cron("quick").last_status, STATUS_OK);
}

#[tokio::test]
async fn run_returns_ok_when_cancelled_before_the_first_poll_completes() {
    let mut h = Harness::start(vec![], Reply::completed()).await;
    h.stop().await.unwrap();
    assert!(h.gateway.requests().is_empty());
}
