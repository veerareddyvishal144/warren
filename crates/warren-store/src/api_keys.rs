//! `api_keys` table. Only the SHA-256 of a key is stored; `hash` is unique.

use rusqlite::{Connection, Row, params};
use warren_core::{ApiKey, Result};

use crate::{
    errors::insert_err,
    rows::{exec, exec_expect, query_many, query_one},
};

const COLS: &str = "id, workspace_id, name, prefix, hash, created_at, expires_at, last_used_at";

fn read(row: &Row<'_>) -> rusqlite::Result<ApiKey> {
    Ok(ApiKey {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        name: row.get("name")?,
        prefix: row.get("prefix")?,
        hash: row.get("hash")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        last_used_at: row.get("last_used_at")?,
    })
}

pub(crate) fn create(conn: &Connection, k: &ApiKey) -> Result<()> {
    exec(
        conn,
        &format!("INSERT INTO api_keys ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
        params![
            k.id,
            k.workspace_id,
            k.name,
            k.prefix,
            k.hash,
            k.created_at,
            k.expires_at,
            k.last_used_at
        ],
    )
    .map_err(insert_err("api key"))?;
    Ok(())
}

pub(crate) fn get_by_hash(conn: &Connection, hash: &str) -> Result<ApiKey> {
    query_one(
        conn,
        &format!("SELECT {COLS} FROM api_keys WHERE hash = ?1"),
        params![hash],
        "api key",
        read,
    )
}

pub(crate) fn list(conn: &Connection, workspace_id: &str) -> Result<Vec<ApiKey>> {
    query_many(
        conn,
        &format!(
            "SELECT {COLS} FROM api_keys WHERE workspace_id = ?1 ORDER BY created_at DESC, rowid DESC"
        ),
        params![workspace_id],
        read,
    )
}

pub(crate) fn delete(conn: &Connection, workspace_id: &str, id: &str) -> Result<()> {
    exec_expect(
        conn,
        "DELETE FROM api_keys WHERE workspace_id = ?1 AND id = ?2",
        params![workspace_id, id],
        "api key",
    )
}

pub(crate) fn touch(conn: &Connection, id: &str, at: i64) -> Result<()> {
    exec_expect(
        conn,
        "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
        params![id, at],
        "api key",
    )
}
