//! `metrics` table: one sample per instance per minute while running.

use rusqlite::{Connection, Row, params};
use warren_core::{MetricPoint, Result};

use crate::{
    errors::map_err,
    rows::{exec, query_many},
};

fn read(row: &Row<'_>) -> rusqlite::Result<MetricPoint> {
    Ok(MetricPoint {
        at: row.get("at")?,
        cpu_percent: row.get("cpu_percent")?,
        memory_bytes: row.get("memory_bytes")?,
        disk_bytes: row.get("disk_bytes")?,
    })
}

pub(crate) fn insert(conn: &Connection, instance_id: &str, p: MetricPoint) -> Result<()> {
    exec(
        conn,
        "INSERT INTO metrics (instance_id, at, cpu_percent, memory_bytes, disk_bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![instance_id, p.at, p.cpu_percent, p.memory_bytes, p.disk_bytes],
    )
    .map_err(map_err)?;
    Ok(())
}

/// Samples at or after `since`, newest first.
pub(crate) fn list(conn: &Connection, instance_id: &str, since: i64) -> Result<Vec<MetricPoint>> {
    query_many(
        conn,
        "SELECT at, cpu_percent, memory_bytes, disk_bytes FROM metrics WHERE instance_id = ?1 AND at >= ?2 \
         ORDER BY at DESC, rowid DESC",
        params![instance_id, since],
        read,
    )
}

/// Drop every sample older than `before`.
pub(crate) fn prune(conn: &Connection, before: i64) -> Result<()> {
    exec(conn, "DELETE FROM metrics WHERE at < ?1", params![before]).map_err(map_err)?;
    Ok(())
}
