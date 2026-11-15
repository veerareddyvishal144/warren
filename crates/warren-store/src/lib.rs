//! SQLite persistence for Warren (rusqlite, bundled).
use std::{path::Path, sync::Arc};
use warren_core::{Error, Result};

pub struct SqliteStore;

impl SqliteStore {
    /// Open or create the database at `path` and run migrations.
    pub fn open(_path: &Path) -> Result<Arc<SqliteStore>> {
        Err(Error::internal("store: not implemented"))
    }
}
