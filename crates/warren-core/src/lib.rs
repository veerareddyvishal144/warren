//! The shared contract of Warren: domain types, async traits every subsystem
//! implements, error codes, shapes, rates, configuration, ids, the signed-URL
//! signer, the API-key authenticator and JSON conventions.
//!
//! Every other crate depends on this one; this one depends on nothing inside the
//! workspace.

pub mod agentapi;
pub mod auth;
pub mod config;
pub mod errors;
pub mod ids;
pub mod json;
pub mod shapes;
pub mod signer;
pub mod traits;
pub mod types;

pub use auth::KeyAuthenticator;
pub use config::{Config, HostKind};
pub use errors::{Error, Result};
pub use signer::HmacSigner;
pub use traits::*;
pub use types::*;
