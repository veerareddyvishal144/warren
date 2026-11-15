//! The fallback handler every request lands in: classify the Host header and
//! hand the request to the API, the dashboard or the instance proxy.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Request, State},
    response::Response,
};
use http::header::HOST;
use tower::ServiceExt;

use crate::{Edge, Route, proxy, route};

/// Dispatch one request by Host header and path.
pub(crate) async fn handle(State(edge): State<Arc<Edge>>, req: Request) -> Response {
    let host = request_host(&req);
    match route(&edge.config, &host, req.uri().path()) {
        Route::Api if is_exec_path(req.uri().path()) => call(edge.api_untimed.clone(), req).await,
        Route::Api => call(edge.api.clone(), req).await,
        Route::Dashboard => call(edge.dashboard.clone(), req).await,
        Route::Instance { id, port } => proxy::handle(edge, req, host, id, port).await,
    }
}

/// `POST /v1/instances/{id}/exec` runs a command for up to its own
/// `timeout_sec`, so it is served without the API deadline.
pub(crate) fn is_exec_path(path: &str) -> bool {
    let mut parts = path.trim_end_matches('/').split('/');
    parts.next() == Some("")
        && parts.next() == Some("v1")
        && parts.next() == Some("instances")
        && parts.next().is_some_and(|id| !id.is_empty())
        && parts.next() == Some("exec")
        && parts.next().is_none()
}

/// The request's authority: the `Host` header, or the URI authority when the
/// client spoke HTTP/2 and sent none. Empty when neither exists.
pub(crate) fn request_host(req: &Request) -> String {
    req.headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(ToString::to_string)
        .or_else(|| req.uri().authority().map(ToString::to_string))
        .unwrap_or_default()
}

/// Call a nested router as a tower service. Routers never fail.
async fn call(router: Router, req: Request) -> Response {
    match router.oneshot(req).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}
