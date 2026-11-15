//! Mapping from `rusqlite` failures to `warren_core::Error`.

use rusqlite::ffi;
use warren_core::Error;

/// True when SQLite rejected a write because a `UNIQUE` or `PRIMARY KEY`
/// constraint would be violated.
pub(crate) fn is_unique_violation(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, _) => {
            f.code == ffi::ErrorCode::ConstraintViolation
                && (f.extended_code == ffi::SQLITE_CONSTRAINT_UNIQUE
                    || f.extended_code == ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
        }
        _ => false,
    }
}

/// Generic mapping: no row -> `not_found`, unique violation -> `conflict`,
/// anything else -> `internal` carrying the SQLite message.
pub(crate) fn map_err(e: rusqlite::Error) -> Error {
    match e {
        rusqlite::Error::QueryReturnedNoRows => Error::not_found("record"),
        e if is_unique_violation(&e) => Error::conflict("record already exists"),
        e => Error::internal(format!("sqlite: {e}")),
    }
}

/// Mapping for inserts of a named entity: a unique violation becomes
/// `conflict("<what> already exists")`; other failures go through [`map_err`].
pub(crate) fn insert_err(what: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |e| {
        if is_unique_violation(&e) {
            Error::conflict(format!("{what} already exists"))
        } else {
            map_err(e)
        }
    }
}
