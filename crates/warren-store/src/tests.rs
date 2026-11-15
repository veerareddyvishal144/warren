//! Unit tests against a real SQLite file in a temporary directory.

use std::{collections::BTreeMap, sync::Arc};

use rusqlite::ffi;
use tempfile::TempDir;
use warren_core::{
    ApiKey, Backup, Budget, Cron, Instance, InstanceStatus, InstanceType, LedgerEntry, LedgerKind,
    MetricPoint, NetworkMode, Shape, Store, Template, TemplateScope, Workspace,
    errors::{CODE_CONFLICT, CODE_INTERNAL, CODE_NOT_FOUND},
};

use crate::{
    SqliteStore,
    errors::{is_unique_violation, map_err},
    migrations,
};

fn open() -> (TempDir, Arc<SqliteStore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open(&dir.path().join("warren.db")).expect("open store");
    (dir, store)
}

fn ws(id: &str, created_at: i64) -> Workspace {
    Workspace {
        id: id.into(),
        name: format!("name of {id}"),
        created_at,
        wallet_micros: 5_000_000,
        top_up_total_micros: 0,
        free_credit_granted: true,
    }
}

fn key(id: &str, workspace_id: &str, created_at: i64) -> ApiKey {
    ApiKey {
        id: id.into(),
        workspace_id: workspace_id.into(),
        name: format!("key {id}"),
        prefix: "sk_live_abcdef".into(),
        hash: format!("hash-{id}"),
        created_at,
        expires_at: created_at + 3600,
        last_used_at: 0,
    }
}

fn tpl(workspace_id: &str, name: &str, revision: u32) -> Template {
    Template {
        name: name.into(),
        scope: if workspace_id.is_empty() {
            TemplateScope::System
        } else {
            TemplateScope::Workspace
        },
        workspace_id: workspace_id.into(),
        image_ref: format!("registry/{name}:v{revision}"),
        image_digest: format!("sha256:{revision:064x}"),
        default_port: 3737,
        mount_path: "/home/agent".into(),
        agent: true,
        description: "a template".into(),
        types: vec![InstanceType::Default, InstanceType::Vm],
        default_shape: Shape {
            cpu: 2,
            memory: 4,
            disk: 4,
        },
        env: BTreeMap::from([("WARREN_AGENT_DRIVER".to_string(), "echo".to_string())]),
        revision,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000 + i64::from(revision),
    }
}

fn inst(id: &str, workspace_id: &str, status: InstanceStatus, created_at: i64) -> Instance {
    Instance {
        id: id.into(),
        workspace_id: workspace_id.into(),
        name: format!("instance {id}"),
        template: "warren-agent@1".into(),
        template_name: "warren-agent".into(),
        template_revision: 1,
        image_ref: "warren/agent:dev".into(),
        kind: InstanceType::Vm,
        status,
        status_reason: "because".into(),
        shape: Shape {
            cpu: 4,
            memory: 8,
            disk: 6,
        },
        region: "local".into(),
        metadata: BTreeMap::from([("plan".to_string(), "pro".to_string())]),
        env: BTreeMap::from([("APP_ENV".to_string(), "production".to_string())]),
        network: NetworkMode::DenyAll,
        auto_sleep: true,
        idle_timeout_seconds: 900,
        budget: Budget {
            monthly_cap_micros: 1_000_000,
            credit_micros: 250_000,
        },
        url: String::new(),
        default_port: 3737,
        mount_path: "/home/agent".into(),
        agent_api: true,
        parent_id: "parent0001".into(),
        created_at,
        updated_at: created_at + 1,
        started_at: created_at + 2,
        stopped_at: created_at + 3,
        slept_at: created_at + 4,
        last_activity_at: created_at + 5,
        meter_started_at: created_at + 6,
        metered_through: created_at + 7,
        meter_carry_micros: 0.375,
        runtime: "firecracker".into(),
        runtime_ref: format!("vm-{id}"),
        ip: "172.30.0.7".into(),
        storage_path: format!("/var/lib/warren/instances/{id}"),
        instance_token: "tok3n".into(),
        disk_used_bytes: 123_456,
    }
}

fn backup(id: &str, instance_id: &str, created_at: i64) -> Backup {
    Backup {
        id: id.into(),
        instance_id: instance_id.into(),
        workspace_id: "ws_a".into(),
        path: format!("/var/lib/warren/backups/{instance_id}/{id}.tar.gz"),
        size_bytes: 4096,
        created_at,
        note: "before upgrade".into(),
    }
}

fn cron(id: &str, instance_id: &str, enabled: bool, next_run_at: i64, created_at: i64) -> Cron {
    Cron {
        id: id.into(),
        instance_id: instance_id.into(),
        workspace_id: "ws_a".into(),
        name: format!("cron {id}"),
        prompt: "summarise the inbox".into(),
        schedule: "*/5 * * * *".into(),
        timezone: "Europe/Dublin".into(),
        enabled,
        last_run_at: 0,
        last_status: String::new(),
        next_run_at,
        created_at,
        updated_at: created_at,
    }
}

fn entry(workspace_id: &str, kind: LedgerKind, micros: i64, at: i64) -> LedgerEntry {
    LedgerEntry {
        id: String::new(),
        workspace_id: workspace_id.into(),
        instance_id: "ab12cd34ef".into(),
        kind,
        micros,
        minutes: 1.5,
        at,
        note: "note".into(),
    }
}

async fn pragma<T>(store: &SqliteStore, name: &'static str) -> T
where
    T: rusqlite::types::FromSql + Send + 'static,
{
    store
        .run(move |c| {
            c.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, T>(0))
                .map_err(map_err)
        })
        .await
        .expect("pragma")
}

#[tokio::test]
async fn open_configures_the_connection_and_migrates_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("deeper").join("warren.db");
    let store = SqliteStore::open(&path).expect("open creates parents");
    assert!(path.exists());
    assert_eq!(store.path(), path.as_path());
    assert!(format!("{store:?}").contains("warren.db"));

    assert_eq!(
        pragma::<String>(&store, "journal_mode")
            .await
            .to_lowercase(),
        "wal"
    );
    assert_eq!(pragma::<i64>(&store, "synchronous").await, 1);
    assert_eq!(pragma::<i64>(&store, "busy_timeout").await, 5000);
    assert_eq!(pragma::<i64>(&store, "foreign_keys").await, 1);

    let version = store.run(|c| migrations::current_version(c)).await.unwrap();
    assert_eq!(version, migrations::MIGRATIONS.last().unwrap().version);
    drop(store);

    // Re-opening applies nothing new and keeps the data.
    let again = SqliteStore::open(&path).expect("reopen");
    let rows: i64 = again
        .run(|c| {
            c.query_row("SELECT COUNT(*) FROM migrations", [], |r| r.get(0))
                .map_err(map_err)
        })
        .await
        .unwrap();
    assert_eq!(rows, migrations::MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn workspace_round_trip() {
    let (_dir, store) = open();
    let a = ws("ws_a", 100);
    let b = ws("ws_b", 200);
    store.create_workspace(&a).await.unwrap();
    store.create_workspace(&b).await.unwrap();
    assert_eq!(store.get_workspace("ws_a").await.unwrap(), a);

    let dup = store.create_workspace(&a).await.unwrap_err();
    assert_eq!(dup.code, CODE_CONFLICT, "{dup}");
    assert_eq!(
        store.get_workspace("ws_zz").await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    let mut a2 = a.clone();
    a2.name = "renamed".into();
    a2.wallet_micros = 42;
    a2.top_up_total_micros = 7;
    a2.free_credit_granted = false;
    store.update_workspace(&a2).await.unwrap();
    assert_eq!(store.get_workspace("ws_a").await.unwrap(), a2);

    let missing = ws("ws_missing", 1);
    assert_eq!(
        store.update_workspace(&missing).await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    let ids: Vec<String> = store
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(ids, vec!["ws_b", "ws_a"], "newest first");
}

#[tokio::test]
async fn api_key_round_trip() {
    let (_dir, store) = open();
    let k1 = key("k1", "ws_a", 100);
    let k2 = key("k2", "ws_a", 200);
    let other = key("k3", "ws_b", 300);
    for k in [&k1, &k2, &other] {
        store.create_api_key(k).await.unwrap();
    }
    assert_eq!(
        store.get_api_key_by_hash("hash-k1").await.unwrap(),
        k1,
        "hash is read back too"
    );
    assert_eq!(
        store.get_api_key_by_hash("nope").await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    let mut same_hash = key("k9", "ws_a", 400);
    same_hash.hash = k1.hash.clone();
    assert_eq!(
        store.create_api_key(&same_hash).await.unwrap_err().code,
        CODE_CONFLICT
    );
    assert_eq!(
        store.create_api_key(&k1).await.unwrap_err().code,
        CODE_CONFLICT
    );

    let ids: Vec<String> = store
        .list_api_keys("ws_a")
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.id)
        .collect();
    assert_eq!(ids, vec!["k2", "k1"]);

    store.touch_api_key("k1", 999).await.unwrap();
    assert_eq!(
        store
            .get_api_key_by_hash("hash-k1")
            .await
            .unwrap()
            .last_used_at,
        999
    );
    assert_eq!(
        store.touch_api_key("k404", 1).await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    assert_eq!(
        store.delete_api_key("ws_b", "k1").await.unwrap_err().code,
        CODE_NOT_FOUND,
        "wrong workspace"
    );
    store.delete_api_key("ws_a", "k1").await.unwrap();
    assert_eq!(
        store.get_api_key_by_hash("hash-k1").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store.delete_api_key("ws_a", "k1").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert_eq!(store.list_api_keys("ws_b").await.unwrap(), vec![other]);
}

#[tokio::test]
async fn templates_visibility_and_ordering() {
    let (_dir, store) = open();
    let sys_b = tpl("", "warren-base", 1);
    let sys_a = tpl("", "warren-agent", 1);
    let own_z = tpl("ws_a", "zeta", 1);
    let own_m = tpl("ws_a", "mango", 1);
    let foreign = tpl("ws_b", "alpha", 1);
    for t in [&sys_b, &sys_a, &own_z, &own_m, &foreign] {
        store.upsert_template(t).await.unwrap();
    }

    assert_eq!(
        store.get_template("ws_a", "warren-base").await.unwrap(),
        sys_b,
        "system visible to all"
    );
    assert_eq!(
        store.get_template("ws_b", "warren-agent").await.unwrap(),
        sys_a
    );
    assert_eq!(store.get_template("", "warren-agent").await.unwrap(), sys_a);
    assert_eq!(store.get_template("ws_a", "mango").await.unwrap(), own_m);
    assert_eq!(
        store.get_template("ws_b", "mango").await.unwrap_err().code,
        CODE_NOT_FOUND,
        "foreign hidden"
    );
    assert_eq!(
        store.get_template("ws_a", "alpha").await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    let names: Vec<String> = store
        .list_templates("ws_a")
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(
        names,
        vec!["warren-agent", "warren-base", "mango", "zeta"],
        "system first, then by name"
    );
    let names: Vec<String> = store
        .list_templates("ws_c")
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["warren-agent", "warren-base"]);

    // A workspace template shadows a system one of the same name for that workspace only.
    let shadow = tpl("ws_a", "warren-base", 1);
    store.upsert_template(&shadow).await.unwrap();
    assert_eq!(
        store.get_template("ws_a", "warren-base").await.unwrap(),
        shadow
    );
    assert_eq!(
        store.get_template("ws_b", "warren-base").await.unwrap(),
        sys_b
    );
}

#[tokio::test]
async fn template_revision_history() {
    let (_dir, store) = open();
    let v1 = tpl("ws_a", "my-agent", 1);
    store.upsert_template(&v1).await.unwrap();
    assert_eq!(store.get_template("ws_a", "my-agent").await.unwrap(), v1);

    // Republishing increments the revision regardless of what the caller passes.
    let mut v2 = tpl("ws_a", "my-agent", 1);
    v2.image_ref = "registry/my-agent:v2".into();
    v2.updated_at = 1_700_000_500;
    store.upsert_template(&v2).await.unwrap();
    let current = store.get_template("ws_a", "my-agent").await.unwrap();
    assert_eq!(current.revision, 2);
    assert_eq!(current.image_ref, "registry/my-agent:v2");
    assert_eq!(
        current,
        Template {
            revision: 2,
            ..v2.clone()
        }
    );

    let mut v3 = tpl("ws_a", "my-agent", 7);
    v3.image_ref = "registry/my-agent:v3".into();
    store.upsert_template(&v3).await.unwrap();
    assert_eq!(
        store
            .get_template("ws_a", "my-agent")
            .await
            .unwrap()
            .revision,
        3
    );

    assert_eq!(
        store
            .get_template_revision("ws_a", "my-agent", 1)
            .await
            .unwrap(),
        v1
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "my-agent", 2)
            .await
            .unwrap(),
        Template { revision: 2, ..v2 }
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "my-agent", 3)
            .await
            .unwrap()
            .image_ref,
        "registry/my-agent:v3"
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "my-agent", 4)
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store
            .get_template_revision("ws_b", "my-agent", 1)
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );

    // A new name with revision 0 starts at 1; with an explicit revision keeps it.
    store
        .upsert_template(&tpl("ws_a", "fresh", 0))
        .await
        .unwrap();
    assert_eq!(
        store.get_template("ws_a", "fresh").await.unwrap().revision,
        1
    );
    store
        .upsert_template(&tpl("ws_a", "pinned", 5))
        .await
        .unwrap();
    assert_eq!(
        store.get_template("ws_a", "pinned").await.unwrap().revision,
        5
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "pinned", 5)
            .await
            .unwrap()
            .revision,
        5
    );

    // System templates are re-seeded in place at the revision given.
    let mut sys = tpl("", "warren-base", 1);
    store.upsert_template(&sys).await.unwrap();
    sys.image_digest = "sha256:new".into();
    store.upsert_template(&sys).await.unwrap();
    assert_eq!(
        store.get_template("ws_a", "warren-base").await.unwrap(),
        sys
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "warren-base", 1)
            .await
            .unwrap(),
        sys
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "warren-base", 2)
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );

    // Delete removes the current row and the whole history, but nothing else.
    store.delete_template("ws_a", "my-agent").await.unwrap();
    assert_eq!(
        store
            .get_template("ws_a", "my-agent")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store
            .get_template_revision("ws_a", "my-agent", 1)
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store
            .delete_template("ws_a", "my-agent")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store
            .delete_template("ws_b", "fresh")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND,
        "wrong workspace"
    );
    assert!(store.get_template("ws_a", "fresh").await.is_ok());
}

#[tokio::test]
async fn instance_round_trip_with_internal_fields() {
    let (_dir, store) = open();
    let a = inst("aaaaaaaaaa", "ws_a", InstanceStatus::Running, 100);
    let b = inst("bbbbbbbbbb", "ws_a", InstanceStatus::Sleeping, 200);
    let c = inst("cccccccccc", "ws_b", InstanceStatus::Stopped, 300);
    for i in [&a, &b, &c] {
        store.create_instance(i).await.unwrap();
    }
    assert_eq!(
        store.get_instance("aaaaaaaaaa").await.unwrap(),
        a,
        "every field, url excepted, survives"
    );
    assert_eq!(
        store
            .get_instance_for_workspace("ws_a", "aaaaaaaaaa")
            .await
            .unwrap(),
        a
    );
    assert_eq!(
        store
            .get_instance_for_workspace("ws_b", "aaaaaaaaaa")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store.get_instance("zzzzzzzzzz").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store.create_instance(&a).await.unwrap_err().code,
        CODE_CONFLICT
    );

    let ids: Vec<String> = store
        .list_instances("ws_a")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec!["bbbbbbbbbb", "aaaaaaaaaa"], "newest first");
    let ids: Vec<String> = store
        .list_all_instances()
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec!["cccccccccc", "bbbbbbbbbb", "aaaaaaaaaa"]);

    let mut a2 = a.clone();
    a2.status = InstanceStatus::Stopped;
    a2.status_reason = "insufficient_balance".into();
    a2.kind = InstanceType::Default;
    a2.network = NetworkMode::AllowAll;
    a2.shape = Shape {
        cpu: 2,
        memory: 4,
        disk: 12,
    };
    a2.metadata.clear();
    a2.env.insert("EXTRA".into(), "1".into());
    a2.ip.clear();
    a2.runtime_ref = "container-xyz".into();
    a2.metered_through = 12_345;
    a2.meter_carry_micros = 0.999_999;
    a2.disk_used_bytes = 1 << 40;
    a2.auto_sleep = false;
    a2.budget = Budget::default();
    store.update_instance(&a2).await.unwrap();
    assert_eq!(store.get_instance("aaaaaaaaaa").await.unwrap(), a2);

    let ghost = inst("gggggggggg", "ws_a", InstanceStatus::Running, 1);
    assert_eq!(
        store.update_instance(&ghost).await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    // Minimal default instance also round-trips.
    let bare = Instance {
        id: "dddddddddd".into(),
        workspace_id: "ws_a".into(),
        ..Default::default()
    };
    store.create_instance(&bare).await.unwrap();
    assert_eq!(store.get_instance("dddddddddd").await.unwrap(), bare);
}

#[tokio::test]
async fn delete_instance_removes_crons_and_metrics_but_keeps_backups() {
    let (_dir, store) = open();
    let a = inst("aaaaaaaaaa", "ws_a", InstanceStatus::Running, 100);
    let b = inst("bbbbbbbbbb", "ws_a", InstanceStatus::Running, 200);
    store.create_instance(&a).await.unwrap();
    store.create_instance(&b).await.unwrap();
    for id in ["aaaaaaaaaa", "bbbbbbbbbb"] {
        store
            .create_cron(&cron(&format!("cron-{id}"), id, true, 10, 1))
            .await
            .unwrap();
        store
            .insert_metric(
                id,
                MetricPoint {
                    at: 10,
                    cpu_percent: 1.0,
                    memory_bytes: 2,
                    disk_bytes: 3,
                },
            )
            .await
            .unwrap();
        store
            .create_backup(&backup(&format!("bk-{id}"), id, 1))
            .await
            .unwrap();
    }

    store.delete_instance("aaaaaaaaaa").await.unwrap();
    assert_eq!(
        store.get_instance("aaaaaaaaaa").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store.delete_instance("aaaaaaaaaa").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert!(store.list_crons("aaaaaaaaaa").await.unwrap().is_empty());
    assert!(
        store
            .list_metrics("aaaaaaaaaa", 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.list_backups("aaaaaaaaaa").await.unwrap().len(),
        1,
        "backup rows wait for the file cleanup"
    );

    assert_eq!(store.list_crons("bbbbbbbbbb").await.unwrap().len(), 1);
    assert_eq!(store.list_metrics("bbbbbbbbbb", 0).await.unwrap().len(), 1);
    assert!(store.get_instance("bbbbbbbbbb").await.is_ok());
}

#[tokio::test]
async fn count_awake_instances_follows_the_core_definition() {
    let (_dir, store) = open();
    for (i, status) in InstanceStatus::ALL.iter().enumerate() {
        store
            .create_instance(&inst(&format!("a{i:09}"), "ws_a", *status, i as i64))
            .await
            .unwrap();
    }
    store
        .create_instance(&inst("bbbbbbbbbb", "ws_b", InstanceStatus::Running, 1))
        .await
        .unwrap();
    let expected = InstanceStatus::ALL.iter().filter(|s| s.awake()).count() as u32;
    assert_eq!(expected, 7, "sanity: the core defines seven awake states");
    assert_eq!(store.count_awake_instances("ws_a").await.unwrap(), expected);
    assert_eq!(store.count_awake_instances("ws_b").await.unwrap(), 1);
    assert_eq!(store.count_awake_instances("ws_c").await.unwrap(), 0);
}

#[tokio::test]
async fn count_instances_using_template_ignores_deleted_rows() {
    let (_dir, store) = open();
    let mut one = inst("aaaaaaaaaa", "ws_a", InstanceStatus::Running, 1);
    one.template_name = "foo".into();
    let mut two = inst("bbbbbbbbbb", "ws_a", InstanceStatus::Sleeping, 2);
    two.template_name = "foo".into();
    let mut gone = inst("cccccccccc", "ws_a", InstanceStatus::Deleted, 3);
    gone.template_name = "foo".into();
    let mut other_ws = inst("dddddddddd", "ws_b", InstanceStatus::Running, 4);
    other_ws.template_name = "foo".into();
    let mut other_tpl = inst("eeeeeeeeee", "ws_a", InstanceStatus::Running, 5);
    other_tpl.template_name = "bar".into();
    for i in [&one, &two, &gone, &other_ws, &other_tpl] {
        store.create_instance(i).await.unwrap();
    }
    assert_eq!(
        store
            .count_instances_using_template("ws_a", "foo")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_instances_using_template("ws_b", "foo")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_instances_using_template("", "foo")
            .await
            .unwrap(),
        3,
        "empty workspace = everywhere"
    );
    assert_eq!(
        store
            .count_instances_using_template("ws_a", "nope")
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ledger_moves_the_wallet_atomically() {
    let (_dir, store) = open();
    let mut w = ws("ws_a", 1);
    w.wallet_micros = 0;
    store.create_workspace(&w).await.unwrap();

    let mut topup = entry("ws_a", LedgerKind::Topup, 5_000_000, 100);
    store.apply_ledger(&mut topup).await.unwrap();
    assert_eq!(topup.id.len(), 32, "id assigned when empty");
    assert!(topup.id.bytes().all(|b| b.is_ascii_hexdigit()));
    let w = store.get_workspace("ws_a").await.unwrap();
    assert_eq!(
        (w.wallet_micros, w.top_up_total_micros),
        (5_000_000, 5_000_000)
    );

    let mut charge = entry("ws_a", LedgerKind::Compute, -1_234, 200);
    charge.id = "fixed-id".into();
    store.apply_ledger(&mut charge).await.unwrap();
    assert_eq!(charge.id, "fixed-id", "given ids are kept");
    let mut credit = entry("ws_a", LedgerKind::Credit, 1_000, 300);
    store.apply_ledger(&mut credit).await.unwrap();
    let w = store.get_workspace("ws_a").await.unwrap();
    assert_eq!(
        (w.wallet_micros, w.top_up_total_micros),
        (5_000_000 - 1_234 + 1_000, 5_000_000)
    );

    let listed = store.list_ledger("ws_a", 0).await.unwrap();
    assert_eq!(
        listed,
        vec![credit.clone(), charge.clone(), topup.clone()],
        "newest first, 0 = no limit"
    );
    assert_eq!(
        store.list_ledger("ws_a", 2).await.unwrap(),
        vec![credit, charge.clone()]
    );
    assert!(store.list_ledger("ws_b", 10).await.unwrap().is_empty());

    // Unknown workspace: nothing is written.
    let mut orphan = entry("ws_none", LedgerKind::Topup, 1, 400);
    assert_eq!(
        store.apply_ledger(&mut orphan).await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert!(store.list_ledger("ws_none", 0).await.unwrap().is_empty());

    // Duplicate id: the wallet update made earlier in the transaction is rolled back.
    let before = store.get_workspace("ws_a").await.unwrap();
    let mut dup = entry("ws_a", LedgerKind::Topup, 999_999, 500);
    dup.id = charge.id.clone();
    assert_eq!(
        store.apply_ledger(&mut dup).await.unwrap_err().code,
        CODE_CONFLICT
    );
    assert_eq!(
        store.get_workspace("ws_a").await.unwrap(),
        before,
        "rolled back"
    );
    assert_eq!(store.list_ledger("ws_a", 0).await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ledger_writes_serialize_on_the_connection() {
    let (_dir, store) = open();
    let mut w = ws("ws_a", 1);
    w.wallet_micros = 0;
    store.create_workspace(&w).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..64 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let mut e = entry("ws_a", LedgerKind::Topup, 1, i);
            store.apply_ledger(&mut e).await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let w = store.get_workspace("ws_a").await.unwrap();
    assert_eq!((w.wallet_micros, w.top_up_total_micros), (64, 64));
    assert_eq!(store.list_ledger("ws_a", 0).await.unwrap().len(), 64);
}

#[tokio::test]
async fn metrics_round_trip_and_prune() {
    let (_dir, store) = open();
    let p = |at: i64| MetricPoint {
        at,
        cpu_percent: at as f64 / 10.0,
        memory_bytes: at * 2,
        disk_bytes: at * 3,
    };
    for at in [100, 200, 300] {
        store.insert_metric("aaaaaaaaaa", p(at)).await.unwrap();
    }
    store.insert_metric("bbbbbbbbbb", p(150)).await.unwrap();

    assert_eq!(
        store.list_metrics("aaaaaaaaaa", 0).await.unwrap(),
        vec![p(300), p(200), p(100)],
        "newest first"
    );
    assert_eq!(
        store.list_metrics("aaaaaaaaaa", 200).await.unwrap(),
        vec![p(300), p(200)],
        "since is inclusive"
    );
    assert!(
        store
            .list_metrics("cccccccccc", 0)
            .await
            .unwrap()
            .is_empty()
    );

    store.prune_metrics(250).await.unwrap();
    assert_eq!(
        store.list_metrics("aaaaaaaaaa", 0).await.unwrap(),
        vec![p(300)]
    );
    assert!(
        store
            .list_metrics("bbbbbbbbbb", 0)
            .await
            .unwrap()
            .is_empty()
    );
    store.prune_metrics(0).await.unwrap();
}

#[tokio::test]
async fn backups_round_trip() {
    let (_dir, store) = open();
    let b1 = backup("b1", "aaaaaaaaaa", 100);
    let b2 = backup("b2", "aaaaaaaaaa", 200);
    let other = backup("b3", "bbbbbbbbbb", 300);
    for b in [&b1, &b2, &other] {
        store.create_backup(b).await.unwrap();
    }
    assert_eq!(
        store.create_backup(&b1).await.unwrap_err().code,
        CODE_CONFLICT
    );
    assert_eq!(
        store.list_backups("aaaaaaaaaa").await.unwrap(),
        vec![b2.clone(), b1.clone()]
    );
    assert_eq!(
        store.get_backup("aaaaaaaaaa", "b1").await.unwrap(),
        b1,
        "path is read back"
    );
    assert_eq!(
        store.get_backup("bbbbbbbbbb", "b1").await.unwrap_err().code,
        CODE_NOT_FOUND,
        "wrong instance"
    );
    assert_eq!(
        store
            .get_backup("aaaaaaaaaa", "nope")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );

    store
        .delete_backups_for_instance("aaaaaaaaaa")
        .await
        .unwrap();
    assert!(store.list_backups("aaaaaaaaaa").await.unwrap().is_empty());
    assert_eq!(store.list_backups("bbbbbbbbbb").await.unwrap(), vec![other]);
    store
        .delete_backups_for_instance("aaaaaaaaaa")
        .await
        .unwrap();
}

#[tokio::test]
async fn crons_round_trip_and_due_selection() {
    let (_dir, store) = open();
    let due = cron("c1", "aaaaaaaaaa", true, 100, 10);
    let disabled = cron("c2", "aaaaaaaaaa", false, 100, 20);
    let later = cron("c3", "aaaaaaaaaa", true, 500, 30);
    let elsewhere = cron("c4", "bbbbbbbbbb", true, 50, 40);
    for c in [&due, &disabled, &later, &elsewhere] {
        store.create_cron(c).await.unwrap();
    }
    assert_eq!(
        store.create_cron(&due).await.unwrap_err().code,
        CODE_CONFLICT
    );
    assert_eq!(store.get_cron("aaaaaaaaaa", "c1").await.unwrap(), due);
    assert_eq!(
        store.get_cron("bbbbbbbbbb", "c1").await.unwrap_err().code,
        CODE_NOT_FOUND,
        "wrong instance"
    );

    let ids: Vec<String> = store
        .list_crons("aaaaaaaaaa")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["c3", "c2", "c1"], "newest first");

    let ids: Vec<String> = store
        .list_due_crons(200)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(
        ids,
        vec!["c4", "c1"],
        "enabled and due, earliest first; disabled excluded"
    );
    let ids: Vec<String> = store
        .list_due_crons(99)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["c4"]);
    let ids: Vec<String> = store
        .list_due_crons(500)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["c4", "c1", "c3"], "due at exactly now counts");

    let mut ran = due.clone();
    ran.last_run_at = 200;
    ran.last_status = "ok".into();
    ran.next_run_at = 700;
    ran.updated_at = 200;
    ran.name = "renamed".into();
    ran.enabled = true;
    store.update_cron(&ran).await.unwrap();
    assert_eq!(store.get_cron("aaaaaaaaaa", "c1").await.unwrap(), ran);
    let ids: Vec<String> = store
        .list_due_crons(600)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["c4", "c3"]);

    let mut wrong_instance = ran.clone();
    wrong_instance.instance_id = "bbbbbbbbbb".into();
    assert_eq!(
        store.update_cron(&wrong_instance).await.unwrap_err().code,
        CODE_NOT_FOUND
    );

    assert_eq!(
        store
            .delete_cron("bbbbbbbbbb", "c1")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
    store.delete_cron("aaaaaaaaaa", "c1").await.unwrap();
    assert_eq!(
        store.get_cron("aaaaaaaaaa", "c1").await.unwrap_err().code,
        CODE_NOT_FOUND
    );
    assert_eq!(
        store
            .delete_cron("aaaaaaaaaa", "c1")
            .await
            .unwrap_err()
            .code,
        CODE_NOT_FOUND
    );
}

#[test]
fn error_mapping() {
    let no_rows = map_err(rusqlite::Error::QueryReturnedNoRows);
    assert_eq!(no_rows.code, CODE_NOT_FOUND);

    let unique = rusqlite::Error::SqliteFailure(
        ffi::Error::new(ffi::SQLITE_CONSTRAINT_UNIQUE),
        Some("dup".into()),
    );
    assert!(is_unique_violation(&unique));
    assert_eq!(map_err(unique).code, CODE_CONFLICT);
    let pk =
        rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_CONSTRAINT_PRIMARYKEY), None);
    assert!(is_unique_violation(&pk));

    let not_null =
        rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_CONSTRAINT_NOTNULL), None);
    assert!(!is_unique_violation(&not_null));
    assert_eq!(map_err(not_null).code, CODE_INTERNAL);

    let busy =
        rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), Some("locked".into()));
    let mapped = map_err(busy);
    assert_eq!(mapped.code, CODE_INTERNAL);
    assert!(
        mapped.details.unwrap()["cause"]
            .as_str()
            .unwrap()
            .contains("locked"),
        "cause kept for logs"
    );
}
