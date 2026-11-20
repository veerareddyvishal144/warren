//! The egress broker: an HTTP forward proxy plus provider routes on the host.
//!
//! Instances with `network: allowlist` have no route out; their proxy
//! variables point at this listener, which forwards plain HTTP requests and
//! `CONNECT` tunnels only to hosts on the instance's `allowed_domains` and
//! only to public addresses. Every instance with secrets also gets one
//! provider route per secret, `http://<broker>/r/<token>/...`: the broker
//! rewrites the path onto the secret's `upstream`, adds `header: value` and
//! streams the answer back, so the credential itself never enters the
//! instance.
//!
//! The calling instance is identified by the source address of the TCP
//! connection, which the lifecycle registers in the [`Registry`] when a
//! runtime reports an instance's address and removes on stop, sleep, failure
//! or delete. Route tokens are therefore only honoured from the address they
//! were registered for. Nothing here is reachable from outside the host
//! unless the operator binds `--broker-listen` to a public interface.
//!
//! Module map: `registry` (the source-address table), `proxy` (`CONNECT` and
//! absolute-form requests), `routes` (provider routes over reqwest with a
//! resolver that refuses private addresses), `server` (the listener, the
//! per-connection service and the JSON error answers).

mod proxy;
mod registry;
mod routes;
mod server;

#[cfg(test)]
mod tests;

use std::{sync::Arc, time::Duration};

pub use registry::Registry;
pub use server::{Broker, serve};

/// Knobs of a [`Broker`]. `Default` is what the daemon runs with.
#[derive(Debug, Clone)]
pub struct Options {
    /// Upper bound on connecting to a destination or an upstream.
    pub connect_timeout: Duration,
    /// How long a destination or upstream may take to answer with headers;
    /// bodies stream without a deadline.
    pub response_timeout: Duration,
    /// Largest request body accepted on a provider route (routes buffer the
    /// request so the upstream sees a `Content-Length`).
    pub max_route_body_bytes: usize,
    /// Connect to loopback, private and other reserved addresses. Only for
    /// tests that run their upstream on 127.0.0.1; the daemon never sets it.
    pub allow_forbidden_destinations: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(120),
            max_route_body_bytes: 16 << 20,
            allow_forbidden_destinations: false,
        }
    }
}

/// A broker over a fresh registry with default options; the registry is what
/// the lifecycle gets as its `BrokerRegistry`.
pub fn new_broker() -> (Arc<Broker>, Arc<Registry>) {
    let registry = Registry::new();
    (
        Arc::new(Broker::new(registry.clone(), Options::default())),
        registry,
    )
}
