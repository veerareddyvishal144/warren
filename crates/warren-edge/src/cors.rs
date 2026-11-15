//! CORS on instance hosts. Browsers send preflights without credentials, so
//! they could never pass instance auth: the edge answers them itself. Every
//! other response on an instance host echoes the request `Origin`.

use axum::{body::Body, response::Response};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{
        ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
        ACCESS_CONTROL_REQUEST_HEADERS, ORIGIN, VARY,
    },
};

/// Headers the contract always allows.
pub(crate) const ALLOW_HEADERS: &str = "X-Warren-Key, Content-Type, Authorization";
/// Methods the contract always allows.
pub(crate) const ALLOW_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
const MAX_AGE: &str = "600";

/// The answer to an `OPTIONS` request on an instance host.
pub(crate) fn preflight(request: &HeaderMap) -> Response {
    let origin = request
        .get(ORIGIN)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("*"));
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let h = resp.headers_mut();
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
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
    h.append(VARY, HeaderValue::from_static("Origin"));
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

/// Echo the Origin on a response (proxied or an edge error) when one was sent.
pub(crate) fn apply(resp: &mut Response, origin: Option<&HeaderValue>) {
    let Some(origin) = origin else { return };
    let h = resp.headers_mut();
    h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(
        ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    h.append(VARY, HeaderValue::from_static("Origin"));
}
