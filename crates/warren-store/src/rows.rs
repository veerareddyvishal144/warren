//! Small helpers shared by the per-entity modules: running statements through
//! the prepared-statement cache, decoding JSON and enum columns, and turning
//! "0 rows changed" into `not_found`.

use std::fmt::Display;

use rusqlite::{Connection, OptionalExtension, Params, Row, types::Type};
use serde::{Serialize, de::DeserializeOwned};
use warren_core::{Error, Result};

use crate::errors::map_err;

/// Execute a statement and return the number of rows it changed.
pub(crate) fn exec<P: Params>(conn: &Connection, sql: &str, params: P) -> rusqlite::Result<usize> {
    conn.prepare_cached(sql)?.execute(params)
}

/// Execute a statement that must touch at least one row; zero rows means the
/// entity named `what` does not exist.
pub(crate) fn exec_expect<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    what: &str,
) -> Result<()> {
    match exec(conn, sql, params).map_err(map_err)? {
        0 => Err(Error::not_found(what)),
        _ => Ok(()),
    }
}

/// Run a query expected to yield at most one row.
pub(crate) fn query_opt<T, P, F>(conn: &Connection, sql: &str, params: P, f: F) -> Result<Option<T>>
where
    P: Params,
    F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
{
    conn.prepare_cached(sql)
        .map_err(map_err)?
        .query_row(params, f)
        .optional()
        .map_err(map_err)
}

/// Run a query expected to yield exactly one row; none means `what` is missing.
pub(crate) fn query_one<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    what: &str,
    f: F,
) -> Result<T>
where
    P: Params,
    F: FnOnce(&Row<'_>) -> rusqlite::Result<T>,
{
    query_opt(conn, sql, params, f)?.ok_or_else(|| Error::not_found(what))
}

/// Run a query and collect every row.
pub(crate) fn query_many<T, P, F>(conn: &Connection, sql: &str, params: P, f: F) -> Result<Vec<T>>
where
    P: Params,
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut stmt = conn.prepare_cached(sql).map_err(map_err)?;
    let rows = stmt.query_map(params, f).map_err(map_err)?;
    rows.collect::<rusqlite::Result<Vec<T>>>().map_err(map_err)
}

/// `SELECT COUNT(*)` style queries.
pub(crate) fn count<P: Params>(conn: &Connection, sql: &str, params: P) -> Result<u32> {
    let n: i64 = conn
        .prepare_cached(sql)
        .map_err(map_err)?
        .query_row(params, |r| r.get(0))
        .map_err(map_err)?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// A column holding a value the domain layer cannot interpret (bad JSON, an
/// unknown enum tag). Reported through `rusqlite::Error` so it flows out of row
/// mappers and ends up as `Error::internal`.
pub(crate) fn bad_column(row: &Row<'_>, name: &str, cause: impl Display) -> rusqlite::Error {
    let idx = row.as_ref().column_index(name).unwrap_or(0);
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        format!("column {name}: {cause}").into(),
    )
}

/// Decode a TEXT column holding JSON.
pub(crate) fn col_json<T: DeserializeOwned>(row: &Row<'_>, name: &str) -> rusqlite::Result<T> {
    let raw: String = row.get(name)?;
    serde_json::from_str(&raw).map_err(|e| bad_column(row, name, e))
}

/// Decode a TEXT column holding one of an enum's string tags.
pub(crate) fn col_parse<T>(
    row: &Row<'_>,
    name: &str,
    parse: fn(&str) -> Option<T>,
) -> rusqlite::Result<T> {
    let raw: String = row.get(name)?;
    parse(&raw).ok_or_else(|| bad_column(row, name, format!("unknown value {raw:?}")))
}

/// Encode a value as the JSON stored in a TEXT column.
pub(crate) fn to_json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(Error::from)
}

/// Build `INSERT INTO <table> (c1, c2, ...) VALUES (:c1, :c2, ...)`.
pub(crate) fn insert_sql(table: &str, cols: &[&str], or_replace: bool) -> String {
    let verb = if or_replace {
        "INSERT OR REPLACE INTO"
    } else {
        "INSERT INTO"
    };
    let names = cols.join(", ");
    let binds = cols
        .iter()
        .map(|c| format!(":{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{verb} {table} ({names}) VALUES ({binds})")
}

/// Build `UPDATE <table> SET c2 = :c2, ... WHERE <where_clause>` for every
/// column except the ones listed in `skip` (normally the key columns).
pub(crate) fn update_sql(table: &str, cols: &[&str], skip: &[&str], where_clause: &str) -> String {
    let sets = cols
        .iter()
        .filter(|c| !skip.contains(c))
        .map(|c| format!("{c} = :{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("UPDATE {table} SET {sets} WHERE {where_clause}")
}

/// Current unix time in seconds, for bookkeeping rows the store writes itself.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
