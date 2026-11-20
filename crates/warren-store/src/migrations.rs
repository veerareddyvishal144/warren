//! Embedded, versioned schema migrations.
//!
//! Each migration runs once inside its own transaction and is recorded in the
//! `migrations` table. New schema changes are appended as a new version; an
//! existing version is never edited.

use rusqlite::{Connection, TransactionBehavior, params};
use warren_core::Result;

use crate::{errors::map_err, rows::now_unix};

/// One schema step.
pub(crate) struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Every migration in ascending version order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: V1_INITIAL,
    },
    Migration {
        version: 2,
        name: "drop_billing",
        sql: V2_DROP_BILLING,
    },
    Migration {
        version: 3,
        name: "egress_allowlist_and_secrets",
        sql: V3_ALLOWLIST_AND_SECRETS,
    },
];

const BOOTSTRAP: &str = "CREATE TABLE IF NOT EXISTS migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    applied_at INTEGER NOT NULL
)";

/// Column block shared by `templates` and `template_revisions`.
macro_rules! template_columns {
    () => {
        "workspace_id TEXT    NOT NULL DEFAULT '',
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
    updated_at   INTEGER NOT NULL DEFAULT 0,"
    };
}

const V1_INITIAL: &str = concat!(
    "CREATE TABLE workspaces (
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
    ",
    template_columns!(),
    "
    PRIMARY KEY (workspace_id, name)
);

CREATE TABLE template_revisions (
    ",
    template_columns!(),
    "
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
"
);

/// Warren is self-hosted and has no monetary model: the ledger and every
/// wallet, budget and meter column go. `ALTER TABLE ... DROP COLUMN` needs
/// SQLite 3.35 or newer (the bundled library is far newer) and refuses a column
/// an index references; none of these is indexed, and `ledger_workspace` goes
/// with its table.
const V2_DROP_BILLING: &str = "DROP TABLE ledger;
ALTER TABLE workspaces DROP COLUMN wallet_micros;
ALTER TABLE workspaces DROP COLUMN top_up_total_micros;
ALTER TABLE workspaces DROP COLUMN free_credit_granted;
ALTER TABLE instances DROP COLUMN budget_monthly_cap_micros;
ALTER TABLE instances DROP COLUMN budget_credit_micros;
ALTER TABLE instances DROP COLUMN meter_started_at;
ALTER TABLE instances DROP COLUMN metered_through;
ALTER TABLE instances DROP COLUMN meter_carry_micros;
";

/// Per-instance egress allowlist (a JSON array of patterns on the instance
/// row) and the write-only secrets the host-side broker injects, one row per
/// instance and name. Values are stored in plaintext like `env`; at-rest
/// encryption is a follow-up.
const V3_ALLOWLIST_AND_SECRETS: &str = "ALTER TABLE instances ADD COLUMN allowed_domains TEXT NOT NULL DEFAULT '[]';
CREATE TABLE instance_secrets (
    instance_id TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    upstream    TEXT    NOT NULL DEFAULT '',
    header      TEXT    NOT NULL DEFAULT '',
    value       TEXT    NOT NULL DEFAULT '',
    env         TEXT    NOT NULL DEFAULT '',
    route_token TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (instance_id, name)
);
";

/// Highest schema version recorded in the database (0 when fresh).
pub(crate) fn current_version(conn: &Connection) -> Result<u32> {
    conn.execute_batch(BOOTSTRAP).map_err(map_err)?;
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM migrations",
            [],
            |r| r.get(0),
        )
        .map_err(map_err)?;
    Ok(u32::try_from(v).unwrap_or(u32::MAX))
}

/// Create the bookkeeping table and apply every migration newer than the
/// recorded version, each in its own transaction.
pub(crate) fn apply(conn: &mut Connection) -> Result<()> {
    let current = current_version(conn)?;
    for m in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_err)?;
        tx.execute_batch(m.sql).map_err(|e| {
            warren_core::Error::internal(format!("migration {} ({}): {e}", m.version, m.name))
        })?;
        tx.execute(
            "INSERT INTO migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![m.version, m.name, now_unix()],
        )
        .map_err(map_err)?;
        tx.commit().map_err(map_err)?;
        tracing::info!(
            version = m.version,
            name = m.name,
            "applied store migration"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_strictly_increasing_from_one() {
        for (expected, m) in (1..).zip(MIGRATIONS.iter()) {
            assert_eq!(m.version, expected, "migration {} out of order", m.name);
        }
    }
}
