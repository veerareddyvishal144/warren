//! `workspaces` table.

use rusqlite::{Connection, Row, params};
use warren_core::{Result, Workspace};

use crate::{
    errors::insert_err,
    rows::{exec, exec_expect, query_many, query_one},
};

const COLS: &str = "id, name, created_at";

fn read(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get("id")?,
        name: row.get("name")?,
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn create(conn: &Connection, ws: &Workspace) -> Result<()> {
    exec(
        conn,
        &format!("INSERT INTO workspaces ({COLS}) VALUES (?1, ?2, ?3)"),
        params![ws.id, ws.name, ws.created_at],
    )
    .map_err(insert_err("workspace"))?;
    Ok(())
}

pub(crate) fn get(conn: &Connection, id: &str) -> Result<Workspace> {
    query_one(
        conn,
        &format!("SELECT {COLS} FROM workspaces WHERE id = ?1"),
        params![id],
        "workspace",
        read,
    )
}

pub(crate) fn update(conn: &Connection, ws: &Workspace) -> Result<()> {
    exec_expect(
        conn,
        "UPDATE workspaces SET name = ?2, created_at = ?3 WHERE id = ?1",
        params![ws.id, ws.name, ws.created_at],
        "workspace",
    )
}

pub(crate) fn list(conn: &Connection) -> Result<Vec<Workspace>> {
    query_many(
        conn,
        &format!("SELECT {COLS} FROM workspaces ORDER BY created_at DESC, rowid DESC"),
        [],
        read,
    )
}
