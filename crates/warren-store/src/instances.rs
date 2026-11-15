//! `instances` table: one column per `Instance` field (maps as JSON text),
//! including the serde-skipped internal fields. `url` is derived by the API
//! layer and is not stored.

use std::sync::LazyLock;

use rusqlite::{Connection, Row, ToSql, TransactionBehavior, named_params, params};
use warren_core::{Budget, Instance, InstanceStatus, InstanceType, NetworkMode, Result, Shape};

use crate::{
    errors::{insert_err, map_err},
    rows::{
        col_json, col_parse, count, exec, exec_expect, insert_sql, query_many, query_one, to_json,
        update_sql,
    },
};

const COLS: &[&str] = &[
    "id",
    "workspace_id",
    "name",
    "template",
    "template_name",
    "template_revision",
    "image_ref",
    "kind",
    "status",
    "status_reason",
    "cpu",
    "memory",
    "disk",
    "region",
    "metadata",
    "env",
    "network",
    "auto_sleep",
    "idle_timeout_seconds",
    "budget_monthly_cap_micros",
    "budget_credit_micros",
    "default_port",
    "mount_path",
    "agent_api",
    "parent_id",
    "created_at",
    "updated_at",
    "started_at",
    "stopped_at",
    "slept_at",
    "last_activity_at",
    "meter_started_at",
    "metered_through",
    "meter_carry_micros",
    "runtime",
    "runtime_ref",
    "ip",
    "storage_path",
    "instance_token",
    "disk_used_bytes",
];

/// The meter position, owned by the biller. `INSERT` writes it, `UPDATE`
/// (`update_instance`) never does; only [`set_meter`] changes it afterwards, so
/// a lifecycle full-row write can never rewind the meter and a biller write can
/// never revert a status or ip.
const METER_COLS: &[&str] = &["metered_through", "meter_carry_micros"];

static SELECT: LazyLock<String> =
    LazyLock::new(|| format!("SELECT {} FROM instances", COLS.join(", ")));
static INSERT: LazyLock<String> = LazyLock::new(|| insert_sql("instances", COLS, false));
static UPDATE: LazyLock<String> = LazyLock::new(|| {
    let skip: Vec<&str> = std::iter::once("id")
        .chain(METER_COLS.iter().copied())
        .collect();
    update_sql("instances", COLS, &skip, "id = :id")
});
/// `UPDATE instances SET metered_through = :metered_through, meter_carry_micros = :meter_carry_micros WHERE id = :id`.
static SET_METER: LazyLock<String> =
    LazyLock::new(|| update_sql("instances", METER_COLS, &[], "id = :id"));
/// [`SET_METER`] with the compare-and-swap condition on the stored position.
static SET_METER_CAS: LazyLock<String> = LazyLock::new(|| {
    update_sql(
        "instances",
        METER_COLS,
        &[],
        "id = :id AND metered_through = :expected_from",
    )
});

/// The `IN (...)` list of statuses that count as awake, derived from the core
/// definition so the two can never drift apart.
static AWAKE_IN: LazyLock<String> = LazyLock::new(|| {
    InstanceStatus::ALL
        .iter()
        .filter(|s| s.awake())
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
});

fn read(row: &Row<'_>) -> rusqlite::Result<Instance> {
    Ok(Instance {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        name: row.get("name")?,
        template: row.get("template")?,
        template_name: row.get("template_name")?,
        template_revision: row.get("template_revision")?,
        image_ref: row.get("image_ref")?,
        kind: col_parse(row, "kind", InstanceType::parse)?,
        status: col_parse(row, "status", InstanceStatus::parse)?,
        status_reason: row.get("status_reason")?,
        shape: Shape {
            cpu: row.get("cpu")?,
            memory: row.get("memory")?,
            disk: row.get("disk")?,
        },
        region: row.get("region")?,
        metadata: col_json(row, "metadata")?,
        env: col_json(row, "env")?,
        network: col_parse(row, "network", NetworkMode::parse)?,
        auto_sleep: row.get("auto_sleep")?,
        idle_timeout_seconds: row.get("idle_timeout_seconds")?,
        budget: Budget {
            monthly_cap_micros: row.get("budget_monthly_cap_micros")?,
            credit_micros: row.get("budget_credit_micros")?,
        },
        url: String::new(),
        default_port: row.get("default_port")?,
        mount_path: row.get("mount_path")?,
        agent_api: row.get("agent_api")?,
        parent_id: row.get("parent_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        started_at: row.get("started_at")?,
        stopped_at: row.get("stopped_at")?,
        slept_at: row.get("slept_at")?,
        last_activity_at: row.get("last_activity_at")?,
        meter_started_at: row.get("meter_started_at")?,
        metered_through: row.get("metered_through")?,
        meter_carry_micros: row.get("meter_carry_micros")?,
        runtime: row.get("runtime")?,
        runtime_ref: row.get("runtime_ref")?,
        ip: row.get("ip")?,
        storage_path: row.get("storage_path")?,
        instance_token: row.get("instance_token")?,
        disk_used_bytes: row.get("disk_used_bytes")?,
    })
}

/// Bind every column of `inst` by name and run `sql` with it. Columns the
/// statement does not mention (the meter columns in [`UPDATE`]) are left unbound.
fn write(conn: &Connection, sql: &str, inst: &Instance) -> rusqlite::Result<usize> {
    let metadata = to_json(&inst.metadata)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?;
    let env = to_json(&inst.env)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?;
    let kind = inst.kind.as_str();
    let status = inst.status.as_str();
    let network = inst.network.as_str();
    let binds: &[(&str, &dyn ToSql)] = named_params! {
        ":id": inst.id,
        ":workspace_id": inst.workspace_id,
        ":name": inst.name,
        ":template": inst.template,
        ":template_name": inst.template_name,
        ":template_revision": inst.template_revision,
        ":image_ref": inst.image_ref,
        ":kind": kind,
        ":status": status,
        ":status_reason": inst.status_reason,
        ":cpu": inst.shape.cpu,
        ":memory": inst.shape.memory,
        ":disk": inst.shape.disk,
        ":region": inst.region,
        ":metadata": metadata,
        ":env": env,
        ":network": network,
        ":auto_sleep": inst.auto_sleep,
        ":idle_timeout_seconds": inst.idle_timeout_seconds,
        ":budget_monthly_cap_micros": inst.budget.monthly_cap_micros,
        ":budget_credit_micros": inst.budget.credit_micros,
        ":default_port": inst.default_port,
        ":mount_path": inst.mount_path,
        ":agent_api": inst.agent_api,
        ":parent_id": inst.parent_id,
        ":created_at": inst.created_at,
        ":updated_at": inst.updated_at,
        ":started_at": inst.started_at,
        ":stopped_at": inst.stopped_at,
        ":slept_at": inst.slept_at,
        ":last_activity_at": inst.last_activity_at,
        ":meter_started_at": inst.meter_started_at,
        ":metered_through": inst.metered_through,
        ":meter_carry_micros": inst.meter_carry_micros,
        ":runtime": inst.runtime,
        ":runtime_ref": inst.runtime_ref,
        ":ip": inst.ip,
        ":storage_path": inst.storage_path,
        ":instance_token": inst.instance_token,
        ":disk_used_bytes": inst.disk_used_bytes,
    };
    let mut stmt = conn.prepare_cached(sql)?;
    for (name, value) in binds {
        if let Some(idx) = stmt.parameter_index(name)? {
            stmt.raw_bind_parameter(idx, value)?;
        }
    }
    stmt.raw_execute()
}

pub(crate) fn create(conn: &Connection, inst: &Instance) -> Result<()> {
    write(conn, &INSERT, inst).map_err(insert_err("instance"))?;
    Ok(())
}

pub(crate) fn get(conn: &Connection, id: &str) -> Result<Instance> {
    query_one(
        conn,
        &format!("{} WHERE id = ?1", *SELECT),
        params![id],
        "instance",
        read,
    )
}

pub(crate) fn get_for_workspace(
    conn: &Connection,
    workspace_id: &str,
    id: &str,
) -> Result<Instance> {
    query_one(
        conn,
        &format!("{} WHERE workspace_id = ?1 AND id = ?2", *SELECT),
        params![workspace_id, id],
        "instance",
        read,
    )
}

pub(crate) fn list(conn: &Connection, workspace_id: &str) -> Result<Vec<Instance>> {
    query_many(
        conn,
        &format!(
            "{} WHERE workspace_id = ?1 ORDER BY created_at DESC, rowid DESC",
            *SELECT
        ),
        params![workspace_id],
        read,
    )
}

pub(crate) fn list_all(conn: &Connection) -> Result<Vec<Instance>> {
    query_many(
        conn,
        &format!("{} ORDER BY created_at DESC, rowid DESC", *SELECT),
        [],
        read,
    )
}

/// Write every column except the meter position (see [`METER_COLS`]).
pub(crate) fn update(conn: &Connection, inst: &Instance) -> Result<()> {
    match write(conn, &UPDATE, inst).map_err(map_err)? {
        0 => Err(warren_core::Error::not_found("instance")),
        _ => Ok(()),
    }
}

/// Write the meter position alone, in one statement. With `expected_from` the
/// write is conditional on the stored `metered_through` still being that value
/// and `Ok(false)` reports a miss; `not_found` when no row has this id at all.
pub(crate) fn set_meter(
    conn: &Connection,
    id: &str,
    metered_through: i64,
    carry_micros: f64,
    expected_from: Option<i64>,
) -> Result<bool> {
    let changed = match expected_from {
        Some(expected_from) => exec(
            conn,
            &SET_METER_CAS,
            named_params! {
                ":metered_through": metered_through,
                ":meter_carry_micros": carry_micros,
                ":id": id,
                ":expected_from": expected_from,
            },
        ),
        None => exec(
            conn,
            &SET_METER,
            named_params! {
                ":metered_through": metered_through,
                ":meter_carry_micros": carry_micros,
                ":id": id,
            },
        ),
    }
    .map_err(map_err)?;
    if changed > 0 {
        return Ok(true);
    }
    // The connection is serialized, so this cannot race the UPDATE above.
    if count(
        conn,
        "SELECT COUNT(*) FROM instances WHERE id = ?1",
        params![id],
    )? == 0
    {
        return Err(warren_core::Error::not_found("instance"));
    }
    Ok(false)
}

/// Remove the instance together with its crons and metric samples. Backup rows
/// are left for `delete_backups_for_instance`, which the caller runs once the
/// archive files are gone.
pub(crate) fn delete(conn: &mut Connection, id: &str) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_err)?;
    exec_expect(
        &tx,
        "DELETE FROM instances WHERE id = ?1",
        params![id],
        "instance",
    )?;
    exec(&tx, "DELETE FROM crons WHERE instance_id = ?1", params![id]).map_err(map_err)?;
    exec(
        &tx,
        "DELETE FROM metrics WHERE instance_id = ?1",
        params![id],
    )
    .map_err(map_err)?;
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// Instances in a status for which `InstanceStatus::awake()` holds.
pub(crate) fn count_awake(conn: &Connection, workspace_id: &str) -> Result<u32> {
    count(
        conn,
        &format!(
            "SELECT COUNT(*) FROM instances WHERE workspace_id = ?1 AND status IN ({})",
            *AWAKE_IN
        ),
        params![workspace_id],
    )
}
