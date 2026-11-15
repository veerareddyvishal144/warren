//! `ledger` table and the wallet movement that goes with every entry.

use rusqlite::{Connection, Row, TransactionBehavior, params};
use warren_core::{Error, LedgerEntry, LedgerKind, Result, ids};

use crate::{
    errors::{insert_err, map_err},
    rows::{col_parse, exec, query_many},
};

const COLS: &str = "id, workspace_id, instance_id, kind, micros, minutes, at, note";

fn read(row: &Row<'_>) -> rusqlite::Result<LedgerEntry> {
    Ok(LedgerEntry {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        instance_id: row.get("instance_id")?,
        kind: col_parse(row, "kind", LedgerKind::parse)?,
        micros: row.get("micros")?,
        minutes: row.get("minutes")?,
        at: row.get("at")?,
        note: row.get("note")?,
    })
}

/// One transaction: move the wallet by `entry.micros` (and the top-up total for
/// `Topup`), then insert the entry. A missing workspace rolls back with
/// `not_found`; a duplicate entry id rolls back with `conflict`. An empty
/// `entry.id` is replaced by a fresh id before anything is written.
pub(crate) fn apply(conn: &mut Connection, entry: &mut LedgerEntry) -> Result<()> {
    if entry.id.is_empty() {
        entry.id = ids::new_id();
    }
    let topup = if entry.kind == LedgerKind::Topup {
        entry.micros
    } else {
        0
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_err)?;
    let moved = exec(
        &tx,
        "UPDATE workspaces SET wallet_micros = wallet_micros + ?1, top_up_total_micros = top_up_total_micros + ?2 \
         WHERE id = ?3",
        params![entry.micros, topup, entry.workspace_id],
    )
    .map_err(map_err)?;
    if moved == 0 {
        return Err(Error::not_found("workspace"));
    }
    exec(
        &tx,
        &format!("INSERT INTO ledger ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
        params![
            entry.id,
            entry.workspace_id,
            entry.instance_id,
            entry.kind.as_str(),
            entry.micros,
            entry.minutes,
            entry.at,
            entry.note
        ],
    )
    .map_err(insert_err("ledger entry"))?;
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// Newest first. `limit` 0 means no limit.
pub(crate) fn list(conn: &Connection, workspace_id: &str, limit: u32) -> Result<Vec<LedgerEntry>> {
    let limit: i64 = if limit == 0 { -1 } else { i64::from(limit) };
    query_many(
        conn,
        &format!(
            "SELECT {COLS} FROM ledger WHERE workspace_id = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2"
        ),
        params![workspace_id, limit],
        read,
    )
}
