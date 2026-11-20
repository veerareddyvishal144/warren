//! `instance_secrets` table: the write-only credentials the broker injects
//! for an instance, one row per instance and name. Replaced as a whole and
//! deleted with the instance; never read by anything that answers clients.
//! Values are stored in plaintext like `env`; at-rest encryption is a
//! follow-up.

use rusqlite::{Connection, Row, TransactionBehavior, params};
use warren_core::{Error, InstanceSecret, Result};

use crate::{
    errors::map_err,
    rows::{exec, query_many},
};

const COLS: &str = "instance_id, name, upstream, header, value, env, route_token, created_at";

fn read(row: &Row<'_>) -> rusqlite::Result<InstanceSecret> {
    Ok(InstanceSecret {
        instance_id: row.get("instance_id")?,
        name: row.get("name")?,
        upstream: row.get("upstream")?,
        header: row.get("header")?,
        value: row.get("value")?,
        env: row.get("env")?,
        route_token: row.get("route_token")?,
        created_at: row.get("created_at")?,
    })
}

/// Delete every secret of `instance_id` and insert `secrets` in one
/// transaction. Every secret must belong to the instance.
pub(crate) fn replace(
    conn: &mut Connection,
    instance_id: &str,
    secrets: &[InstanceSecret],
) -> Result<()> {
    if let Some(stray) = secrets.iter().find(|s| s.instance_id != instance_id) {
        return Err(Error::invalid(format!(
            "secret {} belongs to instance {}, not {instance_id}",
            stray.name, stray.instance_id
        )));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_err)?;
    exec(
        &tx,
        "DELETE FROM instance_secrets WHERE instance_id = ?1",
        params![instance_id],
    )
    .map_err(map_err)?;
    for s in secrets {
        exec(
            &tx,
            &format!(
                "INSERT INTO instance_secrets ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                s.instance_id,
                s.name,
                s.upstream,
                s.header,
                s.value,
                s.env,
                s.route_token,
                s.created_at
            ],
        )
        .map_err(|e| {
            if crate::errors::is_unique_violation(&e) {
                Error::invalid(format!("secret name {} appears twice", s.name))
            } else {
                map_err(e)
            }
        })?;
    }
    tx.commit().map_err(map_err)?;
    Ok(())
}

pub(crate) fn list(conn: &Connection, instance_id: &str) -> Result<Vec<InstanceSecret>> {
    query_many(
        conn,
        &format!("SELECT {COLS} FROM instance_secrets WHERE instance_id = ?1 ORDER BY name"),
        params![instance_id],
        read,
    )
}
