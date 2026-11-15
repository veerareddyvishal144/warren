//! The connection holder and the blocking-pool runner.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use rusqlite::Connection;
use warren_core::{Error, Result};

use crate::{errors::map_err, migrations};

/// SQLite-backed [`warren_core::Store`].
///
/// One connection, serialized by a mutex; every operation runs on Tokio's
/// blocking pool. Cheap to share: clone the `Arc` returned by [`SqliteStore::open`].
pub struct SqliteStore {
    db: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open or create the database at `path`, set the connection pragmas and run
    /// the embedded migrations. Missing parent directories are created.
    pub fn open(path: &Path) -> Result<Arc<SqliteStore>> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::internal(format!("creating {}: {e}", parent.display())))?;
        }
        let mut conn = Connection::open(path)
            .map_err(|e| Error::internal(format!("opening {}: {e}", path.display())))?;
        configure(&conn)?;
        migrations::apply(&mut conn)?;
        tracing::info!(path = %path.display(), "store opened");
        Ok(Arc::new(SqliteStore {
            db: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        }))
    }

    /// Location of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` with exclusive access to the connection on the blocking pool.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock();
            f(&mut conn)
        })
        .await
        .map_err(|e| Error::internal(format!("store task failed: {e}")))?
    }
}

/// WAL for concurrent readers, NORMAL sync (durable across application crashes,
/// a few last transactions at risk on power loss), five seconds of patience for
/// locks held by other processes, and foreign keys on.
fn configure(conn: &Connection) -> Result<()> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
        .map_err(map_err)?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(mode, "sqlite journal_mode is not WAL");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(map_err)?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(map_err)?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(map_err)?;
    Ok(())
}
