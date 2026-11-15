//! `backups` table: restore points and where their archives live.

use rusqlite::{Connection, Row, params};
use warren_core::{Backup, Result};

use crate::{
    errors::{insert_err, map_err},
    rows::{exec, query_many, query_one},
};

const COLS: &str = "id, instance_id, workspace_id, path, size_bytes, created_at, note";

fn read(row: &Row<'_>) -> rusqlite::Result<Backup> {
    Ok(Backup {
        id: row.get("id")?,
        instance_id: row.get("instance_id")?,
        workspace_id: row.get("workspace_id")?,
        path: row.get("path")?,
        size_bytes: row.get("size_bytes")?,
        created_at: row.get("created_at")?,
        note: row.get("note")?,
    })
}

pub(crate) fn create(conn: &Connection, b: &Backup) -> Result<()> {
    exec(
        conn,
        &format!("INSERT INTO backups ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
        params![
            b.id,
            b.instance_id,
            b.workspace_id,
            b.path,
            b.size_bytes,
            b.created_at,
            b.note
        ],
    )
    .map_err(insert_err("backup"))?;
    Ok(())
}

pub(crate) fn list(conn: &Connection, instance_id: &str) -> Result<Vec<Backup>> {
    query_many(
        conn,
        &format!(
            "SELECT {COLS} FROM backups WHERE instance_id = ?1 ORDER BY created_at DESC, rowid DESC"
        ),
        params![instance_id],
        read,
    )
}

pub(crate) fn get(conn: &Connection, instance_id: &str, id: &str) -> Result<Backup> {
    query_one(
        conn,
        &format!("SELECT {COLS} FROM backups WHERE instance_id = ?1 AND id = ?2"),
        params![instance_id, id],
        "backup",
        read,
    )
}

pub(crate) fn delete_for_instance(conn: &Connection, instance_id: &str) -> Result<()> {
    exec(
        conn,
        "DELETE FROM backups WHERE instance_id = ?1",
        params![instance_id],
    )
    .map_err(map_err)?;
    Ok(())
}
