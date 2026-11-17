//! Unit tests against a real SQLite file in a temporary directory.

use std::{collections::BTreeMap, sync::Arc};

use rusqlite::ffi;
use tempfile::TempDir;
use warren_core::{
    ApiKey, Backup, Cron, Instance, InstanceStatus, InstanceType, MetricPoint, NetworkMode, Shape,
    Store, Template, TemplateScope, Workspace,
    errors::{CODE_CONFLICT, CODE_INTERNAL, CODE_NOT_FOUND},
};

use crate::{
    SqliteStore,
    errors::{is_unique_violation, map_err},
    instances, migrations,
    rows::query_many,
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

/// The schema exactly as migration 1 created it, with the billing columns and
/// the ledger. Spelled out here rather than taken from `migrations.rs` so the
/// upgrade is tested against what a development database really contains.
const V1_SCHEMA: &str = "CREATE TABLE migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    applied_at INTEGER NOT NULL
);
INSERT INTO migrations (version, name, applied_at) VALUES (1, 'initial', 1700000000);

CREATE TABLE workspaces (
    id                  TEXT    PRIMARY KEY,
    name                TEXT    NOT NULL DEFAULT '',
    created_at          INTEGER NOT NULL DEFAULT 0,
    wallet_micros       INTEGER NOT NULL DEFAULT 0,
    top_up_total_micros INTEGER NOT NULL DEFAULT 0,
    free_credit_granted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX workspaces_created ON workspaces (created_at);

CREATE TABLE api_keys (
    id           TEXT    PRIMARY KEY,
    workspace_id TEXT    NOT NULL,
    name         TEXT    NOT NULL DEFAULT '',
    prefix       TEXT    NOT NULL DEFAULT '',
    hash         TEXT    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL DEFAULT 0,
    expires_at   INTEGER NOT NULL DEFAULT 0,
    last_used_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX api_keys_workspace ON api_keys (workspace_id, created_at);

CREATE TABLE templates (
    workspace_id TEXT    NOT NULL DEFAULT '',
    name         TEXT    NOT NULL,
    scope        TEXT    NOT NULL,
    image_ref    TEXT    NOT NULL DEFAULT '',
    image_digest TEXT    NOT NULL DEFAULT '',
    default_port INTEGER NOT NULL DEFAULT 0,
    mount_path   TEXT    NOT NULL DEFAULT '',
    agent        INTEGER NOT NULL DEFAULT 0,
    description  TEXT    NOT NULL DEFAULT '',
    types        TEXT    NOT NULL DEFAULT '[]',
    shape_cpu    INTEGER NOT NULL DEFAULT 0,
    shape_memory INTEGER NOT NULL DEFAULT 0,
    shape_disk   INTEGER NOT NULL DEFAULT 0,
    env          TEXT    NOT NULL DEFAULT '{}',
    revision     INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE template_revisions (
    workspace_id TEXT    NOT NULL DEFAULT '',
    name         TEXT    NOT NULL,
    scope        TEXT    NOT NULL,
    image_ref    TEXT    NOT NULL DEFAULT '',
    image_digest TEXT    NOT NULL DEFAULT '',
    default_port INTEGER NOT NULL DEFAULT 0,
    mount_path   TEXT    NOT NULL DEFAULT '',
    agent        INTEGER NOT NULL DEFAULT 0,
    description  TEXT    NOT NULL DEFAULT '',
    types        TEXT    NOT NULL DEFAULT '[]',
    shape_cpu    INTEGER NOT NULL DEFAULT 0,
    shape_memory INTEGER NOT NULL DEFAULT 0,
    shape_disk   INTEGER NOT NULL DEFAULT 0,
    env          TEXT    NOT NULL DEFAULT '{}',
    revision     INTEGER NOT NULL DEFAULT 1,
    created_at   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, name, revision)
);

CREATE TABLE instances (
    id                        TEXT    PRIMARY KEY,
    workspace_id              TEXT    NOT NULL,
    name                      TEXT    NOT NULL DEFAULT '',
    template                  TEXT    NOT NULL DEFAULT '',
    template_name             TEXT    NOT NULL DEFAULT '',
    template_revision         INTEGER NOT NULL DEFAULT 0,
    image_ref                 TEXT    NOT NULL DEFAULT '',
    kind                      TEXT    NOT NULL,
    status                    TEXT    NOT NULL,
    status_reason             TEXT    NOT NULL DEFAULT '',
    cpu                       INTEGER NOT NULL DEFAULT 0,
    memory                    INTEGER NOT NULL DEFAULT 0,
    disk                      INTEGER NOT NULL DEFAULT 0,
    region                    TEXT    NOT NULL DEFAULT '',
    metadata                  TEXT    NOT NULL DEFAULT '{}',
    env                       TEXT    NOT NULL DEFAULT '{}',
    network                   TEXT    NOT NULL,
    auto_sleep                INTEGER NOT NULL DEFAULT 0,
    idle_timeout_seconds      INTEGER NOT NULL DEFAULT 0,
    budget_monthly_cap_micros INTEGER NOT NULL DEFAULT 0,
    budget_credit_micros      INTEGER NOT NULL DEFAULT 0,
    default_port              INTEGER NOT NULL DEFAULT 0,
    mount_path                TEXT    NOT NULL DEFAULT '',
    agent_api                 INTEGER NOT NULL DEFAULT 0,
    parent_id                 TEXT    NOT NULL DEFAULT '',
    created_at                INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL DEFAULT 0,
    started_at                INTEGER NOT NULL DEFAULT 0,
    stopped_at                INTEGER NOT NULL DEFAULT 0,
    slept_at                  INTEGER NOT NULL DEFAULT 0,
    last_activity_at          INTEGER NOT NULL DEFAULT 0,
    meter_started_at          INTEGER NOT NULL DEFAULT 0,
    metered_through           INTEGER NOT NULL DEFAULT 0,
    meter_carry_micros        REAL    NOT NULL DEFAULT 0,
    runtime                   TEXT    NOT NULL DEFAULT '',
    runtime_ref               TEXT    NOT NULL DEFAULT '',
    ip                        TEXT    NOT NULL DEFAULT '',
    storage_path              TEXT    NOT NULL DEFAULT '',
    instance_token            TEXT    NOT NULL DEFAULT '',
    disk_used_bytes           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX instances_workspace ON instances (workspace_id, created_at);
CREATE INDEX instances_status    ON instances (status);
CREATE INDEX instances_template  ON instances (workspace_id, template_name);

CREATE TABLE ledger (
    id           TEXT    PRIMARY KEY,
    workspace_id TEXT    NOT NULL,
    instance_id  TEXT    NOT NULL DEFAULT '',
    kind         TEXT    NOT NULL,
    micros       INTEGER NOT NULL,
    minutes      REAL    NOT NULL DEFAULT 0,
    at           INTEGER NOT NULL,
    note         TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX ledger_workspace ON ledger (workspace_id, at);

CREATE TABLE metrics (
    instance_id  TEXT    NOT NULL,
    at           INTEGER NOT NULL,
    cpu_percent  REAL    NOT NULL DEFAULT 0,
    memory_bytes INTEGER NOT NULL DEFAULT 0,
    disk_bytes   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX metrics_instance_at ON metrics (instance_id, at);

CREATE TABLE backups (
    id           TEXT    PRIMARY KEY,
    instance_id  TEXT    NOT NULL,
    workspace_id TEXT    NOT NULL,
    path         TEXT    NOT NULL DEFAULT '',
    size_bytes   INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT 0,
    note         TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX backups_instance ON backups (instance_id, created_at);

CREATE TABLE crons (
    id           TEXT    PRIMARY KEY,
    instance_id  TEXT    NOT NULL,
    workspace_id TEXT    NOT NULL,
    name         TEXT    NOT NULL DEFAULT '',
    prompt       TEXT    NOT NULL DEFAULT '',
    schedule     TEXT    NOT NULL DEFAULT '',
    timezone     TEXT    NOT NULL DEFAULT '',
    enabled      INTEGER NOT NULL DEFAULT 1,
    last_run_at  INTEGER NOT NULL DEFAULT 0,
    last_status  TEXT    NOT NULL DEFAULT '',
    next_run_at  INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX crons_instance ON crons (instance_id, created_at);
CREATE INDEX crons_due      ON crons (enabled, next_run_at);

INSERT INTO workspaces (id, name, created_at, wallet_micros, top_up_total_micros, free_credit_granted)
VALUES ('ws_old', 'name of ws_old', 100, 5000000, 7000000, 1);

INSERT INTO instances (
    id, workspace_id, name, template, template_name, template_revision, image_ref, kind, status,
    status_reason, cpu, memory, disk, region, metadata, env, network, auto_sleep,
    idle_timeout_seconds, budget_monthly_cap_micros, budget_credit_micros, default_port,
    mount_path, agent_api, parent_id, created_at, updated_at, started_at, stopped_at, slept_at,
    last_activity_at, meter_started_at, metered_through, meter_carry_micros, runtime, runtime_ref,
    ip, storage_path, instance_token, disk_used_bytes
) VALUES (
    'oldinst001', 'ws_old', 'instance oldinst001', 'warren-agent@1', 'warren-agent', 1,
    'warren/agent:dev', 'vm', 'running', 'because', 4, 8, 6, 'local', '{\"plan\":\"pro\"}',
    '{\"APP_ENV\":\"production\"}', 'deny-all', 1, 900, 1000000, 250000, 3737, '/home/agent', 1,
    'parent0001', 100, 101, 102, 103, 104, 105, 106, 107, 0.375, 'firecracker', 'vm-oldinst001',
    '172.30.0.7', '/var/lib/warren/instances/oldinst001', 'tok3n', 123456
);

INSERT INTO ledger (id, workspace_id, instance_id, kind, micros, minutes, at, note)
VALUES ('entry00001', 'ws_old', 'oldinst001', 'topup', 5000000, 0, 100, '');
";

/// Column names of `table` in schema order.
async fn columns(store: &SqliteStore, table: &'static str) -> Vec<String> {
    store
        .run(move |c| {
            query_many(c, &format!("PRAGMA table_info({table})"), [], |r| {
                r.get::<_, String>(1)
            })
        })
        .await
        .expect("table_info")
}

#[tokio::test]
async fn version_1_database_upgrades_to_version_2_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("warren.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(V1_SCHEMA)
        .expect("old schema");

    let store = SqliteStore::open(&path).expect("open upgrades the schema");
    let versions: Vec<i64> = store
        .run(|c| {
            query_many(
                c,
                "SELECT version FROM migrations ORDER BY version",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(versions, vec![1, 2], "version 1 kept, version 2 recorded");

    let ledger_tables: i64 = store
        .run(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('ledger', 'ledger_workspace')",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)
        })
        .await
        .unwrap();
    assert_eq!(ledger_tables, 0, "ledger table and its index are gone");
    assert_eq!(
        columns(&store, "workspaces").await,
        vec!["id", "name", "created_at"]
    );
    let mut inst_cols = columns(&store, "instances").await;
    inst_cols.sort_unstable();
    let mut expected_cols: Vec<String> = instances::COLS.iter().map(|c| c.to_string()).collect();
    expected_cols.sort_unstable();
    assert_eq!(
        inst_cols, expected_cols,
        "instances columns equal the store's column list"
    );

    // Rows written by the old schema read back without their billing fields.
    let old_ws = ws("ws_old", 100);
    assert_eq!(store.get_workspace("ws_old").await.unwrap(), old_ws);
    let old_inst = inst("oldinst001", "ws_old", InstanceStatus::Running, 100);
    assert_eq!(store.get_instance("oldinst001").await.unwrap(), old_inst);
    assert_eq!(store.count_awake_instances("ws_old").await.unwrap(), 1);

    // Updates and inserts work against the upgraded tables.
    let mut renamed = old_ws.clone();
    renamed.name = "renamed".into();
    store.update_workspace(&renamed).await.unwrap();
    assert_eq!(store.get_workspace("ws_old").await.unwrap(), renamed);
    let mut slept = old_inst.clone();
    slept.status = InstanceStatus::Sleeping;
    slept.slept_at = 999;
    store.update_instance(&slept).await.unwrap();
    assert_eq!(store.get_instance("oldinst001").await.unwrap(), slept);
    assert_eq!(store.count_awake_instances("ws_old").await.unwrap(), 0);

    let new_ws = ws("ws_new", 200);
    store.create_workspace(&new_ws).await.unwrap();
    assert_eq!(store.get_workspace("ws_new").await.unwrap(), new_ws);
    let new_inst = inst("newinst001", "ws_new", InstanceStatus::Running, 300);
    store.create_instance(&new_inst).await.unwrap();
    assert_eq!(store.get_instance("newinst001").await.unwrap(), new_inst);
    assert_eq!(
        store.list_workspaces().await.unwrap(),
        vec![new_ws, renamed],
        "newest first"
    );

    // Re-opening applies nothing further.
    drop(store);
    let again = SqliteStore::open(&path).expect("reopen");
    assert_eq!(
        again.run(|c| migrations::current_version(c)).await.unwrap(),
        2
    );
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
    a2.status_reason = "stopped by operator".into();
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
    a2.disk_used_bytes = 1 << 40;
    a2.auto_sleep = false;
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
