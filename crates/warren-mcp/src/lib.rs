//! MCP server that exposes Warren instances to AI assistants.
//!
//! The binary (`src/main.rs`) reads `WARREN_API_URL` and `WARREN_API_KEY`,
//! builds a [`warren_sdk::Client`], wraps it in [`sdk::SdkBackend`] and serves
//! [`WarrenServer`] over stdio with `rmcp`. The library half stays testable
//! without a daemon: every tool talks to the [`backend::Backend`] trait, which
//! unit tests replace with an in-memory fake.

pub mod args;
pub mod backend;
pub mod sdk;
pub mod server;
pub mod settings;

pub use server::WarrenServer;
pub use settings::Settings;
