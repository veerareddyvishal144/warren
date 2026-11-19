//! warren-gateway runs inside every Warren instance. It serves the Agent API
//! (`/v1/responses`, sessions, models), the files API confined to the
//! persistent home directory, the token-protected `/v1/exec` and `/v1/stats`
//! endpoints and the health endpoints on port 3737. It also supervises the browser terminal
//! (`ttyd`) and acts as PID 1 inside Firecracker microVMs. When it runs as
//! root, files it creates in the persistent home belong to the home's owner
//! and agent commands run as that user (see `owner.rs`). In images that do
//! not ship it, the daemon injects the static binary and runs it in wrap
//! mode around the image's own entrypoint (see `wrap.rs`).
//!
//! The binary in `main.rs` is a thin wrapper around [`server::serve`] and
//! [`init::run_init`]; everything else lives in this library so the HTTP
//! surface can be exercised by integration tests without Docker.

pub mod config;
pub mod drivers;
pub mod exec;
pub mod extract;
pub mod files;
pub mod health;
// PID 1 duties (mounts, hostname, network, power-off) only exist on Linux;
// other hosts get a stub so the gateway itself still builds, for example when
// it runs inside a macOS virtual machine.
#[cfg_attr(not(target_os = "linux"), path = "init_stub.rs")]
pub mod init;
pub mod owner;
pub mod responses;
pub mod server;
pub mod sessions;
pub mod stats;
pub mod terminal;
pub mod wrap;

pub use config::GatewayConfig;
pub use owner::HomeOwner;
pub use server::{AppState, SharedState};
pub use wrap::{WrapSpec, WrappedStatus};

/// Version reported by `/v1/health` and `/v1/version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
