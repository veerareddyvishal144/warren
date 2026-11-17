//! SQLite persistence for Warren (rusqlite, bundled).
//!
//! [`SqliteStore`] implements [`warren_core::Store`] on a single `rusqlite`
//! connection guarded by a `parking_lot::Mutex`. Every trait method runs its SQL
//! on Tokio's blocking pool (`spawn_blocking`), so the async runtime never waits
//! on SQLite. The database is opened in WAL mode with `synchronous=NORMAL`, a
//! five second busy timeout and foreign keys enabled; the schema is created and
//! upgraded by embedded, versioned migrations recorded in a `migrations` table.
//!
//! Error mapping follows the contract in `warren_core::traits`: a missing row is
//! `Error::not_found`, a unique or primary-key violation is `Error::conflict`,
//! and everything else is `Error::internal` with the SQLite cause in `details`.

mod api_keys;
mod backups;
mod crons;
mod errors;
mod instances;
mod metrics;
mod migrations;
mod rows;
mod store;
mod store_impl;
mod templates;
mod workspaces;

#[cfg(test)]
mod tests;

pub use store::SqliteStore;
