//! CORS on instance hosts. Browsers send preflights without credentials, so
//! they could never pass instance auth: the edge answers them itself. Only
//! trusted origins are allowed: the dashboard (the root host) and the
//! instance's own origins. Any other `Origin` gets no
//! `Access-Control-Allow-Origin` at all, so a page on another origin (another
//! instance included) can neither read instance responses nor use the
//! session cookie from a script.

use axum::{body::Body, response::Response};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{
        ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
        ACCESS_CONTROL_REQUEST_HEADERS, ORIGIN, VARY,
    },
};
use warren_core::Config;

/// Headers the contract always allows.
pub(crate) const ALLOW_HEADERS: &str = "X-Warren-Key, Content-Type, Authorization";
/// Methods the contract always allows.
pub(crate) const ALLOW_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
const MAX_AGE: &str = "600";

/// `scheme://<base>[:port]`: the dashboard's origin, with the public port
/// unless it is the scheme's default (the same rule as `Config::instance_url`).
pub(crate) fn root_origin(cfg: &Config) -> String {
    let implicit = cfg.public_port == 0
        || (cfg.public_scheme == "http" && cfg.public_port == 80)
        || (cfg.public_scheme == "https" && cfg.public_port == 443);
    if implicit {
        format!("{}://{}", cfg.public_scheme, cfg.base_domain)
    } else {
        format!(
            "{}://{}:{}",
            cfg.public_scheme, cfg.base_domain, cfg.public_port
        )
    }
}

/// The origins an instance host trusts: the dashboard and the instance's own
/// URLs for the requested host (`instance_url(id)`, plus `preview_url(id,
/// port)` on a port host). Lowercase, no trailing slash.
pub(crate) fn allowed_origins(cfg: &Config, id: &str, host_port: u16) -> Vec<String> {
    let mut origins = vec![root_origin(cfg), cfg.instance_url(id)];
    if host_port != 0 {
        origins.push(cfg.preview_url(id, host_port));
    }
    origins
        .into_iter()
        .map(|o| o.to_ascii_lowercase())
        .collect()
}

/// Whether `origin` (an `Origin` header value) is one of `allowed`.
pub(crate) fn origin_allowed(origin: &HeaderValue, allowed: &[String]) -> bool {
    origin
        .to_str()
        .ok()
        .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
        .is_some_and(|o| allowed.contains(&o))
}

/// The answer to an `OPTIONS` request on an instance host: `204`, with the
/// CORS grant only when the Origin is trusted.
pub(crate) fn preflight(request: &HeaderMap, allowed: &[String]) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let h = resp.headers_mut();
    h.append(VARY, HeaderValue::from_static("Origin"));
    let Some(origin) = request.get(ORIGIN).filter(|o| origin_allowed(o, allowed)) else {
        return resp;
    };
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(ACCESS_CONTROL_ALLOW_HEADERS, allow_headers(request));
    h.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOW_METHODS),
    );
    h.insert(
        ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    h.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE));
    resp
}

/// The contract's headers plus whatever the browser asked for, so an app
/// behind an instance may use its own custom headers.
fn allow_headers(request: &HeaderMap) -> HeaderValue {
    let listed = |name: &str| {
        ALLOW_HEADERS
            .split(',')
            .any(|a| a.trim().eq_ignore_ascii_case(name))
    };
    let mut allowed = ALLOW_HEADERS.to_string();
    let requested = request
        .get_all(ACCESS_CONTROL_REQUEST_HEADERS)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|h| !h.is_empty() && !listed(h));
    for name in requested {
        allowed.push_str(", ");
        allowed.push_str(name);
    }
    HeaderValue::from_str(&allowed).unwrap_or_else(|_| HeaderValue::from_static(ALLOW_HEADERS))
}

/// Grant CORS on a response (proxied or an edge error) when the request came
/// from a trusted Origin; otherwise only `Vary: Origin` is added.
pub(crate) fn apply(resp: &mut Response, origin: Option<&HeaderValue>, allowed: &[String]) {
    let Some(origin) = origin else { return };
    let h = resp.headers_mut();
    h.append(VARY, HeaderValue::from_static("Origin"));
    if !origin_allowed(origin, allowed) {
        return;
    }
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(
        ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
}
