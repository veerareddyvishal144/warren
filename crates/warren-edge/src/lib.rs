//! The single front door of Warren: Host-header dispatch, instance
//! authentication (API key, signed URL, session cookie), wake-on-request and a
//! streaming reverse proxy with WebSocket/upgrade passthrough and CORS.
//!
//! [`router`] returns one axum [`Router`] that is served on the single
//! listener. Its fallback handler reads the `Host` header, classifies it with
//! [`Config::parse_host`] and dispatches:
//!
//! - `api.<base>` goes to the Sandbox API router;
//! - `<base>` goes to the dashboard, except `/v1/...` which goes to the API;
//! - `<id>.<base>` and `<id>-<port>.<base>` are authenticated, the instance is
//!   woken with `Lifecycle::ensure_awake`, and the request is proxied to
//!   `http://<ip>:<port>` with bodies streamed in both directions.

mod auth;
mod cors;
mod dispatch;
mod headers;
mod proxy;
mod tunnel;

#[cfg(test)]
mod tests;

use std::{sync::Arc, time::Duration};

use axum::{Router, body::Body};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioTimer},
};
use warren_core::{Authenticator, Config, HostKind, Lifecycle, Signer};

/// Everything the edge needs. Built by `warrend`.
pub struct Deps {
    pub config: Config,
    /// The Sandbox API (`/v1`), served on `api.<base>` and under `<base>/v1`.
    pub api: Router,
    /// The dashboard, served on the bare base domain.
    pub dashboard: Router,
    pub lifecycle: Arc<dyn Lifecycle>,
    pub auth: Arc<dyn Authenticator>,
    pub signer: Arc<dyn Signer>,
}

/// Tunables of the proxy. [`router`] uses [`EdgeOptions::default`]; tests and
/// operators with unusual workloads use [`router_with`].
#[derive(Debug, Clone)]
pub struct EdgeOptions {
    /// TCP connect timeout to an instance. Exceeding it answers `504 timeout`.
    pub connect_timeout: Duration,
    /// How long an instance may take to send its response headers. Exceeding
    /// it answers `504 timeout`. Bodies stream without a deadline so SSE and
    /// long downloads are never cut.
    pub response_timeout: Duration,
    /// How often an upgraded (WebSocket) tunnel that moved bytes since the
    /// last beat calls `Lifecycle::record_activity`.
    pub activity_heartbeat: Duration,
    /// How long idle upstream connections stay in the pool.
    pub pool_idle_timeout: Duration,
}

impl Default for EdgeOptions {
    fn default() -> Self {
        EdgeOptions {
            connect_timeout: Duration::from_secs(10),
            // A non-streaming agent turn may run for minutes; match the exec
            // ceiling rather than cutting legitimate long requests.
            response_timeout: Duration::from_secs(u64::from(warren_core::DEFAULT_EXEC_TIMEOUT_SEC)),
            activity_heartbeat: Duration::from_secs(30),
            pool_idle_timeout: Duration::from_secs(90),
        }
    }
}

/// Where a request goes after Host and path classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The Sandbox API router.
    Api,
    /// The dashboard router.
    Dashboard,
    /// Authenticated proxying to an instance; `port` 0 means its default port.
    Instance { id: String, port: u16 },
}

/// Classify a Host header exactly as the edge does ([`Config::parse_host`]).
pub fn classify(cfg: &Config, host: &str) -> HostKind {
    cfg.parse_host(host)
}

/// The full dispatch decision for a Host header and request path: the root
/// host serves the dashboard except for `/v1` and everything under it.
pub fn route(cfg: &Config, host: &str, path: &str) -> Route {
    match classify(cfg, host) {
        HostKind::Api => Route::Api,
        HostKind::Root if path == "/v1" || path.starts_with("/v1/") => Route::Api,
        HostKind::Root => Route::Dashboard,
        HostKind::Instance { id, port } => Route::Instance { id, port },
    }
}

/// Shared state behind the fallback handler.
pub(crate) struct Edge {
    pub(crate) config: Config,
    pub(crate) api: Router,
    pub(crate) dashboard: Router,
    pub(crate) lifecycle: Arc<dyn Lifecycle>,
    pub(crate) auth: Arc<dyn Authenticator>,
    pub(crate) signer: Arc<dyn Signer>,
    /// One pooled HTTP/1.1 client for every instance.
    pub(crate) client: Client<HttpConnector, Body>,
    pub(crate) opts: EdgeOptions,
}

/// The root router served on the single listener, with default options.
pub fn router(d: Deps) -> Router {
    router_with(d, EdgeOptions::default())
}

/// The root router served on the single listener, with explicit options.
pub fn router_with(d: Deps, opts: EdgeOptions) -> Router {
    let mut connector = HttpConnector::new();
    connector.enforce_http(true);
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(opts.connect_timeout));
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(opts.pool_idle_timeout)
        .pool_timer(TokioTimer::new())
        .timer(TokioTimer::new())
        .build(connector);
    let edge = Arc::new(Edge {
        config: d.config,
        api: d.api,
        dashboard: d.dashboard,
        lifecycle: d.lifecycle,
        auth: d.auth,
        signer: d.signer,
        client,
        opts,
    });
    Router::new().fallback(dispatch::handle).with_state(edge)
}
