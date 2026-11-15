//! `workspaces` table.

use rusqlite::{Connection, Row, params};
use warren_core::{Result, Workspace};

use crate::{
    errors::insert_err,
    rows::{exec, exec_expect, query_many, query_one},
};

const COLS: &str = "id, name, created_at, wallet_micros, top_up_total_micros, free_credit_granted";

fn read(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get("id")?,
        name: row.get("name")?,
        created_at: row.get("created_at")?,
        wallet_micros: row.get("wallet_micros")?,
        top_up_total_micros: row.get("top_up_total_micros")?,
        free_credit_granted: row.get("free_credit_granted")?,
    })
}

pub(crate) fn create(conn: &Connection, ws: &Workspace) -> Result<()> {
    exec(
        conn,
        &format!("INSERT INTO workspaces ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
        params![
            ws.id,
            ws.name,
            ws.created_at,
            ws.wallet_micros,
            ws.top_up_total_micros,
            ws.free_credit_granted
        ],
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
        "UPDATE workspaces SET name = ?2, created_at = ?3, wallet_micros = ?4, top_up_total_micros = ?5, \
         free_credit_granted = ?6 WHERE id = ?1",
        params![
            ws.id,
            ws.name,
            ws.created_at,
            ws.wallet_micros,
            ws.top_up_total_micros,
            ws.free_credit_granted
        ],
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
