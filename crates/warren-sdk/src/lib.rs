//! Rust client for the Warren Sandbox API and Agent API.
//!
//! * [`Client`] talks to the daemon's Sandbox API (`/v1` on the API host):
//!   signup, workspace, keys, billing, templates, instances and their
//!   lifecycle, exec, logs, metrics, signed URLs, backups and crons.
//! * [`AgentClient`] talks to the gateway inside one instance through the edge:
//!   responses (plain and streamed as [`SseEvent`]s), sessions, models, files
//!   and the token-protected exec endpoint.
//!
//! Both send the workspace key as `Authorization: Bearer` and `x-warren-key`
//! so either plane accepts it. Every call returns [`SdkError`], which separates
//! server answers ([`SdkError::Api`], the decoded error envelope) from transport
//! failures and decode problems; `warren_core::Error: From<SdkError>` lets
//! callers that return `warren_core::Result` use `?`.
//!
//! ```no_run
//! # async fn demo() -> Result<(), warren_sdk::SdkError> {
//! use futures::StreamExt;
//! use warren_core::agentapi::ResponsesRequest;
//! use warren_core::{CreateInstanceRequest, InstanceStatus};
//!
//! let api = warren_sdk::Client::from_env();
//! let req = CreateInstanceRequest { template: Some("warren-agent".into()), ..Default::default() };
//! let inst = api.create_instance(&req).await?;
//! let inst = api.wait_for_status(&inst.id, InstanceStatus::Running, std::time::Duration::from_secs(120)).await?;
//!
//! let agent = api.agent_for(&inst);
//! let mut events = agent.stream(&ResponsesRequest { input: "hello".into(), ..Default::default() });
//! while let Some(ev) = events.next().await {
//!     let ev = ev?;
//!     if let Some(delta) = ev.str_field("delta") {
//!         print!("{delta}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Wire types that exist only on the wire (signup, billing, status answers,
//! cron requests...) live in [`types`]; everything else is `warren_core`.

pub mod agent;
pub mod client;
pub mod error;
pub mod http;
pub mod sse;
pub mod types;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod testserver;

use std::net::SocketAddr;

pub use agent::AgentClient;
pub use client::{Client, DEFAULT_API_URL, ENV_API_KEY, ENV_API_URL, POLL_INTERVAL};
pub use error::SdkError;
pub use sse::{EventStream, SseEvent, SseParser};
pub use types::*;
/// The shared contract, re-exported so callers need not depend on it directly.
pub use warren_core;

/// A builder with this crate's defaults: rustls, a 10 s connect timeout, the
/// `warren-sdk/<version>` user agent and no overall timeout (streams and long
/// exec calls set their own per request).
pub fn http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(http::CONNECT_TIMEOUT)
        .user_agent(http::USER_AGENT)
}

/// The HTTP client [`Client::new`] and [`AgentClient::new`] use.
///
/// # Panics
/// Only if the TLS backend cannot be initialised, like `reqwest::Client::new`.
pub fn http_client() -> reqwest::Client {
    http_builder()
        .build()
        .expect("reqwest client: TLS backend initialisation failed")
}

/// A client whose DNS resolution for the given hosts is pinned to the given
/// addresses, for tests that serve `api.warren.localhost` or
/// `<id>.warren.localhost` from a local listener. The port of each address is
/// ignored by reqwest; the URL's port is used.
///
/// # Panics
/// Only if the TLS backend cannot be initialised; see
/// [`try_client_with_resolve`] for the fallible form.
pub fn client_with_resolve(host_to_addr: &[(&str, SocketAddr)]) -> reqwest::Client {
    try_client_with_resolve(host_to_addr)
        .expect("reqwest client: TLS backend initialisation failed")
}

/// [`client_with_resolve`] without the panic.
pub fn try_client_with_resolve(
    host_to_addr: &[(&str, SocketAddr)],
) -> Result<reqwest::Client, SdkError> {
    let mut b = http_builder();
    for (host, addr) in host_to_addr {
        b = b.resolve(host, *addr);
    }
    Ok(b.build()?)
}
