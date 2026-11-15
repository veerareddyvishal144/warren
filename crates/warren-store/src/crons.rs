//! `crons` table: scheduled prompts for agent instances.

use rusqlite::{Connection, Row, params};
use warren_core::{Cron, Result};

use crate::{
    errors::insert_err,
    rows::{exec, exec_expect, query_many, query_one},
};

const COLS: &str = "id, instance_id, workspace_id, name, prompt, schedule, timezone, enabled, last_run_at, \
                    last_status, next_run_at, created_at, updated_at";

fn read(row: &Row<'_>) -> rusqlite::Result<Cron> {
    Ok(Cron {
        id: row.get("id")?,
        instance_id: row.get("instance_id")?,
        workspace_id: row.get("workspace_id")?,
        name: row.get("name")?,
        prompt: row.get("prompt")?,
        schedule: row.get("schedule")?,
        timezone: row.get("timezone")?,
        enabled: row.get("enabled")?,
        last_run_at: row.get("last_run_at")?,
        last_status: row.get("last_status")?,
        next_run_at: row.get("next_run_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn create(conn: &Connection, c: &Cron) -> Result<()> {
    exec(
        conn,
        &format!("INSERT INTO crons ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
        params![
            c.id,
            c.instance_id,
            c.workspace_id,
            c.name,
            c.prompt,
            c.schedule,
            c.timezone,
            c.enabled,
            c.last_run_at,
            c.last_status,
            c.next_run_at,
            c.created_at,
            c.updated_at
        ],
    )
    .map_err(insert_err("cron"))?;
    Ok(())
}

pub(crate) fn get(conn: &Connection, instance_id: &str, id: &str) -> Result<Cron> {
    query_one(
        conn,
        &format!("SELECT {COLS} FROM crons WHERE instance_id = ?1 AND id = ?2"),
        params![instance_id, id],
        "cron",
        read,
    )
}

pub(crate) fn list(conn: &Connection, instance_id: &str) -> Result<Vec<Cron>> {
    query_many(
        conn,
        &format!(
            "SELECT {COLS} FROM crons WHERE instance_id = ?1 ORDER BY created_at DESC, rowid DESC"
        ),
        params![instance_id],
        read,
    )
}

/// Enabled crons whose `next_run_at` is at or before `now`, earliest first.
pub(crate) fn list_due(conn: &Connection, now: i64) -> Result<Vec<Cron>> {
    query_many(
        conn,
        &format!(
            "SELECT {COLS} FROM crons WHERE enabled = 1 AND next_run_at <= ?1 ORDER BY next_run_at ASC, rowid ASC"
        ),
        params![now],
        read,
    )
}

/// Rewrite every mutable field; the cron must exist under the same instance.
pub(crate) fn update(conn: &Connection, c: &Cron) -> Result<()> {
    exec_expect(
        conn,
        "UPDATE crons SET workspace_id = ?3, name = ?4, prompt = ?5, schedule = ?6, timezone = ?7, enabled = ?8, \
         last_run_at = ?9, last_status = ?10, next_run_at = ?11, created_at = ?12, updated_at = ?13 \
         WHERE id = ?1 AND instance_id = ?2",
        params![
            c.id,
            c.instance_id,
            c.workspace_id,
            c.name,
            c.prompt,
            c.schedule,
            c.timezone,
            c.enabled,
            c.last_run_at,
            c.last_status,
            c.next_run_at,
            c.created_at,
            c.updated_at
        ],
        "cron",
    )
}

pub(crate) fn delete(conn: &Connection, instance_id: &str, id: &str) -> Result<()> {
    exec_expect(
        conn,
        "DELETE FROM crons WHERE instance_id = ?1 AND id = ?2",
        params![instance_id, id],
        "cron",
    )
}
