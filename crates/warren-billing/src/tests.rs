//! Behavioural tests of the biller against the in-memory fakes.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;
use warren_core::{
    Biller as BillerApi, Clock, Config, InstanceStatus, InstanceType, LedgerKind,
    MICROS_PER_DOLLAR, Money, Rates, Shape,
    errors::{
        CODE_INSTANCE_LIMIT_REACHED, CODE_INSUFFICIENT_BALANCE, CODE_INTERNAL,
        CODE_INVALID_REQUEST, CODE_NOT_FOUND,
    },
    shapes::valid_shapes,
};

use crate::{
    Biller, STOP_REASON_INSUFFICIENT_BALANCE,
    fakes::{FakeStore, FixedClock, RecordingStopper, instance, workspace},
};

const T0: i64 = 1_800_000_000;
const USD: Money = MICROS_PER_DOLLAR;
const SMALL: Shape = Shape {
    cpu: 2,
    memory: 4,
    disk: 4,
};
const SMALL_MONTHLY: Money = 4_760_000;
const SMALL_DISK_MONTHLY: Money = 360_000;
const MINUTES_IN_MONTH: i64 = 43_800;

fn setup_with(cfg: Config, now: i64) -> (Arc<Biller>, Arc<FakeStore>, Arc<FixedClock>) {
    let store = FakeStore::new();
    let clock = FixedClock::new(now);
    let biller = Biller::new(store.clone(), cfg, clock.clone());
    (biller, store, clock)
}

fn setup(now: i64) -> (Arc<Biller>, Arc<FakeStore>, Arc<FixedClock>) {
    setup_with(Config::default(), now)
}

fn charged(store: &FakeStore, instance_id: &str) -> Money {
    store
        .ledger_for_instance(instance_id)
        .iter()
        .map(|e| -e.micros)
        .sum()
}

// ---------------------------------------------------------------- pricing

#[test]
fn pricing_math_for_the_four_shapes() {
    let (b, _, _) = setup(T0);
    assert_eq!(b.rates(), Rates::default());
    let shapes = valid_shapes();
    assert_eq!(shapes.len(), 4);
    let expected_running = [SMALL_MONTHLY, 9_340_000, 15_480_000, 27_760_000];
    let expected_disk = [SMALL_DISK_MONTHLY, 540_000, 1_080_000, 2_160_000];
    for ((shape, running), disk) in shapes.iter().zip(expected_running).zip(expected_disk) {
        assert_eq!(
            b.monthly_running_micros(*shape, InstanceType::Default),
            running,
            "{shape}"
        );
        assert_eq!(b.monthly_disk_micros(*shape), disk, "{shape}");
    }
    assert_eq!(shapes[0], SMALL);
    // The vm multiplier applies to compute only: (1.6M + 2.8M) * 2.5 + 360k.
    assert_eq!(
        b.monthly_running_micros(SMALL, InstanceType::Vm),
        11_360_000
    );
    assert_eq!(
        b.monthly_running_micros(SMALL, InstanceType::Performance),
        17_960_000
    );
    // A bigger disk on the same cpu/memory only adds the disk rate.
    assert_eq!(
        b.monthly_running_micros(Shape { disk: 12, ..SMALL }, InstanceType::Default),
        SMALL_MONTHLY + 8 * 90_000
    );
}

#[test]
fn custom_rates_are_used() {
    let cfg = Config {
        rates: Rates {
            vcpu_month_micros: 1_000_000,
            ram_gb_month_micros: 100_000,
            disk_gb_month_micros: 10_000,
            vm_multiplier: 2.0,
            performance_multiplier: 3.0,
        },
        ..Config::default()
    };
    let (b, _, _) = setup_with(cfg.clone(), T0);
    assert_eq!(b.rates(), cfg.rates);
    assert_eq!(
        b.monthly_running_micros(SMALL, InstanceType::Default),
        2_000_000 + 400_000 + 40_000
    );
    assert_eq!(
        b.monthly_running_micros(SMALL, InstanceType::Vm),
        2 * 2_400_000 + 40_000
    );
    assert_eq!(b.monthly_disk_micros(SMALL), 40_000);
}

// ---------------------------------------------------------------- metering

#[tokio::test]
async fn minute_ticks_over_a_month_sum_to_the_monthly_price() {
    let (b, store, clock) = setup(T0);
    store.put_workspace(workspace("ws_a", 1_000 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Sleeping,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    for i in 1..=MINUTES_IN_MONTH {
        clock.set(T0 + i * 60);
        b.tick(clock.now_unix()).await.unwrap();
    }

    let running = charged(&store, "aaaaaaaaa1");
    let sleeping = charged(&store, "aaaaaaaaa2");
    assert!(
        (running - SMALL_MONTHLY).abs() <= 1,
        "running charged {running}"
    );
    assert!(
        (sleeping - SMALL_DISK_MONTHLY).abs() <= 1,
        "sleeping charged {sleeping}"
    );

    let compute = store.ledger_for_instance("aaaaaaaaa1");
    assert_eq!(
        compute.len(),
        MINUTES_IN_MONTH as usize,
        "108.67 micros a minute yields an entry every tick"
    );
    assert!(compute.iter().all(|e| e.kind == LedgerKind::Compute
        && e.micros < 0
        && e.workspace_id == "ws_a"
        && !e.id.is_empty()));
    assert!(
        (compute.iter().map(|e| e.minutes).sum::<f64>() - MINUTES_IN_MONTH as f64).abs() < 1e-9
    );
    let disk = store.ledger_for_instance("aaaaaaaaa2");
    assert!(
        disk.iter()
            .all(|e| e.kind == LedgerKind::Disk && e.micros < 0)
    );

    for id in ["aaaaaaaaa1", "aaaaaaaaa2"] {
        let inst = store.instance(id);
        assert_eq!(inst.metered_through, T0 + MINUTES_IN_MONTH * 60);
        assert!(
            (0.0..1.0).contains(&inst.meter_carry_micros),
            "{}",
            inst.meter_carry_micros
        );
        assert_eq!(
            inst.status,
            store.instance(id).status,
            "the biller never moves status"
        );
    }
    assert_eq!(
        store.workspace("ws_a").wallet_micros,
        1_000 * USD - running - sleeping
    );
}

#[tokio::test]
async fn sub_micro_per_minute_rates_accumulate_through_the_carry() {
    // 2 GB at 10,000 micros per GB-month = 20,000 a month = 0.4566 micros a minute.
    let cfg = Config {
        rates: Rates {
            disk_gb_month_micros: 10_000,
            ..Rates::default()
        },
        ..Config::default()
    };
    let (b, store, clock) = setup_with(cfg, T0);
    store.put_workspace(workspace("ws_a", 10 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Stopped,
        Shape {
            cpu: 2,
            memory: 4,
            disk: 2,
        },
        InstanceType::Default,
        T0,
    ));
    for i in 1..=MINUTES_IN_MONTH {
        clock.set(T0 + i * 60);
        b.tick(clock.now_unix()).await.unwrap();
    }
    let total = charged(&store, "aaaaaaaaa1");
    assert!((total - 20_000).abs() <= 1, "charged {total}");
    let entries = store.ledger_for_instance("aaaaaaaaa1");
    assert!(
        entries.len() < MINUTES_IN_MONTH as usize,
        "ticks below a micro write no entry"
    );
    assert!(
        entries.iter().all(|e| e.micros == -1),
        "each entry is exactly one micro"
    );
    assert_eq!(
        store.instance("aaaaaaaaa1").metered_through,
        T0 + MINUTES_IN_MONTH * 60,
        "the meter still advances on zero-charge ticks"
    );
}

#[tokio::test]
async fn irregular_tick_spacing_still_sums_to_the_monthly_price() {
    let (b, store, clock) = setup(T0);
    store.put_workspace(workspace("ws_a", 1_000 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    let mut minutes = 0;
    let mut ticks = 0;
    while minutes < MINUTES_IN_MONTH {
        minutes += 1 + ticks % 5; // 1,2,3,4,5 minute gaps: 15 minutes per cycle, 2,920 cycles
        ticks += 1;
        // Thirty seconds of jitter past the minute must stay unmetered.
        clock.set(T0 + minutes * 60 + 30);
        b.tick(clock.now_unix()).await.unwrap();
    }
    assert_eq!(minutes, MINUTES_IN_MONTH);
    let total = charged(&store, "aaaaaaaaa1");
    assert!((total - SMALL_MONTHLY).abs() <= 1, "charged {total}");
    let entries = store.ledger_for_instance("aaaaaaaaa1");
    assert_eq!(entries.len(), ticks as usize);
    assert!(
        (entries.iter().map(|e| e.minutes).sum::<f64>() - MINUTES_IN_MONTH as f64).abs() < 1e-9
    );
    assert_eq!(
        store.instance("aaaaaaaaa1").metered_through,
        T0 + MINUTES_IN_MONTH * 60
    );
}

#[tokio::test]
async fn vm_instances_bill_the_multiplied_compute_rate() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Vm,
        T0,
    ));
    b.tick(T0 + 600).await.unwrap();
    // 11,360,000 / 43,800 * 10 minutes = 2,593.6 micros.
    assert_eq!(charged(&store, "aaaaaaaaa1"), 2_593);
    let e = &store.ledger_for_instance("aaaaaaaaa1")[0];
    assert_eq!(e.kind, LedgerKind::Compute);
    assert!((e.minutes - 10.0).abs() < 1e-9);
    assert_eq!(e.at, T0 + 600);
}

#[tokio::test]
async fn tick_skips_instances_that_bill_nothing() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    // Never reached running: free even though it is running now.
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        0,
    ));
    for (id, status) in [
        ("aaaaaaaaa2", InstanceStatus::Provisioning),
        ("aaaaaaaaa3", InstanceStatus::Failed),
        ("aaaaaaaaa4", InstanceStatus::Deleting),
        ("aaaaaaaaa5", InstanceStatus::Deleted),
    ] {
        store.put_instance(instance(
            id,
            "ws_a",
            status,
            SMALL,
            InstanceType::Default,
            T0,
        ));
    }
    // Less than a minute elapsed.
    store.put_instance(instance(
        "aaaaaaaaa6",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0 + 3_600 - 59,
    ));
    // Meter position in the future (clock skew).
    store.put_instance(instance(
        "aaaaaaaaa7",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0 + 7_200,
    ));

    b.tick(T0 + 3_600).await.unwrap();
    assert!(store.ledger().is_empty());
    assert_eq!(store.workspace("ws_a").wallet_micros, 100 * USD);
    for i in 1..=7 {
        let id = format!("aaaaaaaaa{i}");
        let inst = store.instance(&id);
        assert_eq!(inst.metered_through, inst.meter_started_at, "{id}");
        assert!(inst.meter_carry_micros.abs() < 1e-12, "{id}");
    }
}

#[tokio::test]
async fn tick_treats_zero_metered_through_as_meter_started_at() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    let mut inst = instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    );
    inst.metered_through = 0;
    store.put_instance(inst);
    b.tick(T0 + 150).await.unwrap();
    let entries = store.ledger_for_instance("aaaaaaaaa1");
    assert_eq!(entries.len(), 1);
    assert!((entries[0].minutes - 2.0).abs() < 1e-9);
    assert_eq!(entries[0].micros, -217);
    let inst = store.instance("aaaaaaaaa1");
    assert_eq!(
        inst.metered_through,
        T0 + 120,
        "the remaining 30 seconds stay unmetered"
    );
    // The leftover seconds are picked up once they make a whole minute.
    b.tick(T0 + 185).await.unwrap();
    assert_eq!(store.ledger_for_instance("aaaaaaaaa1").len(), 2);
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0 + 180);
}

#[tokio::test]
async fn ledger_failure_leaves_the_meter_untouched_and_is_reported() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Sleeping,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store
        .fail_ledger
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let err = b.tick(T0 + 60).await.unwrap_err();
    assert_eq!(err.code, CODE_INTERNAL);
    assert!(
        err.details.unwrap()["cause"]
            .as_str()
            .unwrap()
            .contains("2 of 2")
    );
    assert!(store.ledger().is_empty());
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0);

    // Once the store recovers the whole span is charged exactly once.
    store
        .fail_ledger
        .store(false, std::sync::atomic::Ordering::SeqCst);
    b.tick(T0 + 120).await.unwrap();
    let entries = store.ledger_for_instance("aaaaaaaaa1");
    assert_eq!(entries.len(), 1);
    assert!((entries[0].minutes - 2.0).abs() < 1e-9);
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0 + 120);
}

#[tokio::test]
async fn meter_save_failure_reverses_the_charge() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store
        .fail_update_instance
        .store(true, std::sync::atomic::Ordering::SeqCst);

    assert_eq!(b.tick(T0 + 60).await.unwrap_err().code, CODE_INTERNAL);
    let ledger = store.ledger();
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger[0].kind, LedgerKind::Compute);
    assert_eq!(ledger[0].micros, -108);
    assert_eq!(ledger[1].kind, LedgerKind::Adjustment);
    assert_eq!(ledger[1].micros, 108);
    assert_eq!(ledger[1].instance_id, "aaaaaaaaa1");
    assert!(ledger[1].note.contains("rollback"));
    assert_eq!(
        store.workspace("ws_a").wallet_micros,
        100 * USD,
        "the wallet is whole again"
    );
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0);

    store
        .fail_update_instance
        .store(false, std::sync::atomic::Ordering::SeqCst);
    b.tick(T0 + 60).await.unwrap();
    assert_eq!(store.workspace("ws_a").wallet_micros, 100 * USD - 108);
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0 + 60);
}

#[tokio::test]
async fn metering_does_not_overwrite_a_status_change_made_meanwhile() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    // The lifecycle moves the instance between the biller's listing and its save.
    store.set_status_on_read("aaaaaaaaa1", InstanceStatus::Stopping);
    b.tick(T0 + 60).await.unwrap();
    let inst = store.instance("aaaaaaaaa1");
    assert_eq!(inst.status, InstanceStatus::Stopping);
    assert_eq!(inst.metered_through, T0 + 60);
    assert_eq!(
        charged(&store, "aaaaaaaaa1"),
        108,
        "the minute was still billed at the rate observed"
    );
}

#[tokio::test]
async fn instance_deleted_during_metering_keeps_its_final_charge() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    // Simulate the deletion racing the tick: the listing sees it, the re-read does not.
    store.set_status_on_read("aaaaaaaaa1", InstanceStatus::Deleted);
    let store2 = store.clone();
    let b2 = b.clone();
    // Remove the row after the listing but before the re-read by racing on the yield in list_all_instances.
    let remover = async move {
        tokio::task::yield_now().await;
        store2.put_instance(instance(
            "zzzzzzzzz9",
            "ws_a",
            InstanceStatus::Deleted,
            SMALL,
            InstanceType::Default,
            0,
        ));
        let _ = warren_core::Store::delete_instance(&*store2, "aaaaaaaaa1").await;
    };
    let (res, ()) = tokio::join!(b2.tick(T0 + 60), remover);
    res.unwrap();
    assert_eq!(charged(&store, "aaaaaaaaa1"), 108);
    assert_eq!(store.workspace("ws_a").wallet_micros, 100 * USD - 108);
}

#[tokio::test]
async fn concurrent_ticks_never_double_charge() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    let (r1, r2) = tokio::join!(b.tick(T0 + 600), b.tick(T0 + 600));
    r1.unwrap();
    r2.unwrap();
    assert_eq!(store.ledger().len(), 1);
    assert_eq!(charged(&store, "aaaaaaaaa1"), 1_086);
}

// ---------------------------------------------------------------- exhausted wallets

#[tokio::test]
async fn exhausted_wallet_stops_compute_billing_instances_only() {
    let (b, store, _) = setup(T0);
    let stopper = Arc::new(RecordingStopper::default());
    b.set_stopper(stopper.clone());
    store.put_workspace(workspace("ws_a", 100, 0));
    store.put_workspace(workspace("ws_b", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Sleeping,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa3",
        "ws_a",
        InstanceStatus::Starting,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa4",
        "ws_a",
        InstanceStatus::Stopped,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "bbbbbbbbb1",
        "ws_b",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));

    b.tick(T0 + 60).await.unwrap();

    assert!(
        store.workspace("ws_a").wallet_micros <= 0,
        "{}",
        store.workspace("ws_a").wallet_micros
    );
    let calls = stopper.calls();
    let ids: BTreeSet<&str> = calls.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, BTreeSet::from(["aaaaaaaaa1", "aaaaaaaaa3"]));
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|(_, reason)| reason == STOP_REASON_INSUFFICIENT_BALANCE)
    );
    assert_eq!(STOP_REASON_INSUFFICIENT_BALANCE, "insufficient_balance");
}

#[tokio::test]
async fn wallet_exactly_zero_counts_as_exhausted() {
    let (b, store, _) = setup(T0);
    let stopper = Arc::new(RecordingStopper::default());
    b.set_stopper(stopper.clone());
    store.put_workspace(workspace("ws_a", 108, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    b.tick(T0 + 60).await.unwrap();
    assert_eq!(store.workspace("ws_a").wallet_micros, 0);
    assert_eq!(stopper.calls().len(), 1);
}

#[tokio::test]
async fn positive_wallets_are_left_alone() {
    let (b, store, _) = setup(T0);
    let stopper = Arc::new(RecordingStopper::default());
    b.set_stopper(stopper.clone());
    store.put_workspace(workspace("ws_a", 109, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    b.tick(T0 + 60).await.unwrap();
    assert_eq!(store.workspace("ws_a").wallet_micros, 1);
    assert!(stopper.calls().is_empty());
}

#[tokio::test]
async fn stopper_failures_are_logged_and_the_rest_still_stop() {
    let (b, store, _) = setup(T0);
    let stopper = Arc::new(RecordingStopper::default());
    stopper.fail_for("aaaaaaaaa1");
    b.set_stopper(stopper.clone());
    store.put_workspace(workspace("ws_a", 1, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    b.tick(T0 + 60).await.unwrap();
    let ids: BTreeSet<String> = stopper.calls().into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        ids,
        BTreeSet::from(["aaaaaaaaa1".to_string(), "aaaaaaaaa2".to_string()])
    );
}

#[tokio::test]
async fn exhausted_wallet_without_a_stopper_is_tolerated() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 1, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    b.tick(T0 + 60).await.unwrap();
    assert!(store.workspace("ws_a").wallet_micros < 0);
    assert_eq!(store.instance("aaaaaaaaa1").status, InstanceStatus::Running);
}

// ---------------------------------------------------------------- limits and wallet rules

#[test]
fn awake_limit_ladder() {
    let (b, _, _) = setup(T0);
    for (top_ups, limit) in [
        (0, 3),
        (1, 10),
        (5 * USD, 10),
        (50 * USD - 1, 10),
        (50 * USD, 50),
        (250 * USD - 1, 50),
        (250 * USD, 200),
        (1_000 * USD - 1, 200),
        (1_000 * USD, 1_000),
        (25_000 * USD, 1_000),
    ] {
        assert_eq!(
            b.awake_limit(&workspace("ws_a", 0, top_ups)),
            limit,
            "top-ups {top_ups}"
        );
    }
    // The wallet balance itself is irrelevant to the ladder.
    assert_eq!(b.awake_limit(&workspace("ws_a", 5_000 * USD, 0)), 3);
}

#[tokio::test]
async fn assert_can_create_requires_one_day_of_the_running_rate() {
    let (b, store, _) = setup(T0);
    let required = SMALL_MONTHLY / 30;
    assert_eq!(required, 158_666);
    store.put_workspace(workspace("ws_a", 0, 0));

    let err = b
        .assert_can_create(
            &workspace("ws_a", required - 1, 0),
            SMALL,
            InstanceType::Default,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INSUFFICIENT_BALANCE);
    assert_eq!(err.status, 402);
    let details = err.details.unwrap();
    assert_eq!(details["required_micros"], required);
    assert_eq!(details["balance_micros"], required - 1);

    b.assert_can_create(
        &workspace("ws_a", required, 0),
        SMALL,
        InstanceType::Default,
    )
    .await
    .unwrap();

    // A vm costs 2.5x on compute, so the same wallet is no longer enough.
    let err = b
        .assert_can_create(&workspace("ws_a", required, 0), SMALL, InstanceType::Vm)
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INSUFFICIENT_BALANCE);
    assert_eq!(err.details.unwrap()["required_micros"], 11_360_000 / 30);

    // A negative wallet is short as well.
    assert_eq!(
        b.assert_can_create(&workspace("ws_a", -5, 0), SMALL, InstanceType::Default)
            .await
            .unwrap_err()
            .code,
        CODE_INSUFFICIENT_BALANCE
    );
}

#[tokio::test]
async fn assert_can_create_enforces_the_awake_limit() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Provisioning,
        SMALL,
        InstanceType::Default,
        0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa3",
        "ws_a",
        InstanceStatus::Sleeping,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa4",
        "ws_a",
        InstanceStatus::Stopped,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    // Another workspace's instances never count.
    store.put_workspace(workspace("ws_b", 100 * USD, 0));
    store.put_instance(instance(
        "bbbbbbbbb1",
        "ws_b",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));

    // Two awake out of three: fine.
    b.assert_can_create(&store.workspace("ws_a"), SMALL, InstanceType::Default)
        .await
        .unwrap();

    store.put_instance(instance(
        "aaaaaaaaa5",
        "ws_a",
        InstanceStatus::Waking,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    let err = b
        .assert_can_create(&store.workspace("ws_a"), SMALL, InstanceType::Default)
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INSTANCE_LIMIT_REACHED);
    assert_eq!(err.status, 409);
    let details = err.details.unwrap();
    assert_eq!(details["limit"], 3);
    assert_eq!(details["awake"], 3);

    // The balance check comes first.
    let err = b
        .assert_can_create(&workspace("ws_a", 0, 0), SMALL, InstanceType::Default)
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INSUFFICIENT_BALANCE);

    // The first top-up lifts the limit to ten.
    let ws = b.top_up("ws_a", 5 * USD, "").await.unwrap();
    assert_eq!(b.awake_limit(&ws), 10);
    b.assert_can_create(&ws, SMALL, InstanceType::Default)
        .await
        .unwrap();
}

#[tokio::test]
async fn assert_can_wake_checks_only_the_limit() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 0, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa2",
        "ws_a",
        InstanceStatus::Restarting,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store.put_instance(instance(
        "aaaaaaaaa3",
        "ws_a",
        InstanceStatus::Sleeping,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    // An empty wallet does not block a wake.
    b.assert_can_wake(&store.workspace("ws_a")).await.unwrap();

    store.put_instance(instance(
        "aaaaaaaaa4",
        "ws_a",
        InstanceStatus::Updating,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    let err = b
        .assert_can_wake(&store.workspace("ws_a"))
        .await
        .unwrap_err();
    assert_eq!(err.code, CODE_INSTANCE_LIMIT_REACHED);
    assert_eq!(err.details.unwrap()["awake"], 3);
}

#[tokio::test]
async fn top_up_bounds_and_ledger() {
    let (b, store, clock) = setup(T0);
    store.put_workspace(workspace("ws_a", 0, 0));
    clock.set(T0 + 42);

    for bad in [
        0,
        -5 * USD,
        5 * USD - 1,
        10_000 * USD + 1,
        Money::MAX,
        Money::MIN,
    ] {
        let err = b.top_up("ws_a", bad, "").await.unwrap_err();
        assert_eq!(err.code, CODE_INVALID_REQUEST, "{bad}");
        assert_eq!(err.status, 400);
    }
    assert_eq!(
        b.top_up("ws_missing", 5 * USD, "").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert!(store.ledger().is_empty(), "rejected top-ups write nothing");

    let ws = b.top_up("ws_a", 5 * USD, "dev").await.unwrap();
    assert_eq!(ws.id, "ws_a");
    assert_eq!(ws.wallet_micros, 5 * USD);
    assert_eq!(ws.top_up_total_micros, 5 * USD);
    assert_eq!(ws, store.workspace("ws_a"));
    let ws = b.top_up("ws_a", 10_000 * USD, "card").await.unwrap();
    assert_eq!(ws.wallet_micros, 10_005 * USD);
    assert_eq!(ws.top_up_total_micros, 10_005 * USD);

    let ledger = store.ledger();
    assert_eq!(ledger.len(), 2);
    let first = &ledger[0];
    assert_eq!(first.kind, LedgerKind::Topup);
    assert_eq!(first.micros, 5 * USD);
    assert_eq!(first.note, "dev");
    assert_eq!(first.workspace_id, "ws_a");
    assert!(first.instance_id.is_empty());
    assert!(first.minutes.abs() < 1e-12);
    assert_eq!(first.at, T0 + 42);
    assert!(!first.id.is_empty());
    assert_eq!(ledger[1].note, "card");

    let (ws, entry) = b.apply_top_up("ws_a", 7 * USD, "with entry").await.unwrap();
    assert_eq!(ws.wallet_micros, 10_012 * USD);
    assert_eq!(entry.micros, 7 * USD);
    assert_eq!(entry.note, "with entry");
    assert_eq!(entry.id, store.ledger()[2].id);
}

#[tokio::test]
async fn grant_credit_takes_any_positive_amount_and_skips_the_ladder() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 0, 0));
    for bad in [0, -1, -5 * USD] {
        assert_eq!(
            b.grant_credit("ws_a", bad, "").await.unwrap_err().code,
            CODE_INVALID_REQUEST,
            "{bad}"
        );
    }
    assert_eq!(
        b.grant_credit("ws_missing", 1, "").await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    let ws = b.grant_credit("ws_a", 1, "one micro").await.unwrap();
    assert_eq!(ws.wallet_micros, 1);
    let ws = b.grant_credit("ws_a", 500 * USD, "promo").await.unwrap();
    assert_eq!(ws.wallet_micros, 500 * USD + 1);
    assert_eq!(ws.top_up_total_micros, 0, "credits never climb the ladder");
    assert_eq!(b.awake_limit(&ws), 3);
    assert!(
        !ws.free_credit_granted,
        "plain credits do not set the signup flag"
    );

    let ledger = store.ledger();
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|e| e.kind == LedgerKind::Credit));
    assert_eq!(ledger[1].note, "promo");

    let (ws, entry) = b.apply_credit("ws_a", 3, "").await.unwrap();
    assert_eq!(ws.wallet_micros, 500 * USD + 4);
    assert_eq!(entry.kind, LedgerKind::Credit);
}

#[tokio::test]
async fn grant_free_credit_happens_once() {
    let (b, store, _) = setup(T0);
    store.put_workspace(workspace("ws_a", 0, 0));
    let ws = b.grant_free_credit("ws_a").await.unwrap();
    assert_eq!(ws.wallet_micros, 5 * USD);
    assert!(ws.free_credit_granted);
    assert_eq!(ws, store.workspace("ws_a"));
    let again = b.grant_free_credit("ws_a").await.unwrap();
    assert_eq!(again, ws);
    let ledger = store.ledger();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].kind, LedgerKind::Credit);
    assert_eq!(ledger[0].note, "free credit");
    assert_eq!(
        b.grant_free_credit("ws_missing").await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    // With no free credit configured only the flag moves.
    let cfg = Config {
        free_credit_micros: 0,
        ..Config::default()
    };
    let (b, store, _) = setup_with(cfg, T0);
    store.put_workspace(workspace("ws_b", 0, 0));
    let ws = b.grant_free_credit("ws_b").await.unwrap();
    assert!(ws.free_credit_granted);
    assert_eq!(ws.wallet_micros, 0);
    assert!(store.ledger().is_empty());
}

// ---------------------------------------------------------------- run loop

#[tokio::test]
async fn run_ticks_on_the_interval_until_cancelled() {
    let cfg = Config {
        meter_interval: Duration::from_millis(20),
        ..Config::default()
    };
    let (b, store, clock) = setup_with(cfg, T0 + 60);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));

    let token = CancellationToken::new();
    let handle = tokio::spawn(b.clone().run(token.clone()));

    // The first tick fires immediately and charges the minute already elapsed.
    for _ in 0..500 {
        if !store.ledger().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(store.ledger().len(), 1);
    assert_eq!(store.instance("aaaaaaaaa1").metered_through, T0 + 60);

    // Later ticks pick up the clock as it moves.
    clock.set(T0 + 180);
    for _ in 0..500 {
        if store.ledger().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(store.ledger().len(), 2);
    assert!((store.ledger()[1].minutes - 2.0).abs() < 1e-9);

    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run must return once cancelled")
        .unwrap()
        .unwrap();
    // Nothing runs after shutdown.
    clock.set(T0 + 3_600);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(store.ledger().len(), 2);
}

#[tokio::test]
async fn run_survives_tick_errors() {
    let cfg = Config {
        meter_interval: Duration::from_millis(10),
        ..Config::default()
    };
    let (b, store, _) = setup_with(cfg, T0 + 60);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    store
        .fail_ledger
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let token = CancellationToken::new();
    let handle = tokio::spawn(b.clone().run(token.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(store.ledger().is_empty());
    store
        .fail_ledger
        .store(false, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..500 {
        if !store.ledger().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        store.ledger().len(),
        1,
        "the loop kept ticking after the failures"
    );
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn run_with_a_zero_interval_falls_back_instead_of_panicking() {
    let cfg = Config {
        meter_interval: Duration::ZERO,
        ..Config::default()
    };
    let (b, store, _) = setup_with(cfg, T0 + 60);
    store.put_workspace(workspace("ws_a", 100 * USD, 0));
    store.put_instance(instance(
        "aaaaaaaaa1",
        "ws_a",
        InstanceStatus::Running,
        SMALL,
        InstanceType::Default,
        T0,
    ));
    let token = CancellationToken::new();
    let handle = tokio::spawn(b.clone().run(token.clone()));
    for _ in 0..500 {
        if !store.ledger().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        store.ledger().len(),
        1,
        "the immediate first tick still runs"
    );
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn run_returns_promptly_when_cancelled_before_starting() {
    let (b, _, _) = setup(T0);
    let token = CancellationToken::new();
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), b.run(token))
        .await
        .unwrap()
        .unwrap();
}
