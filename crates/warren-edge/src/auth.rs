//! Who may reach an instance URL: an API key in `X-Warren-Key` or
//! `Authorization: Bearer`, a signed URL (`?warren_token=`) that becomes the
//! `warren_session` cookie, or that cookie on later requests.

use axum::{body::Body, response::Response};
use http::{
    HeaderMap, HeaderValue, Method, StatusCode, Uri,
    header::{LOCATION, ORIGIN, REFERER, SET_COOKIE},
};
use warren_core::{COOKIE_NAME, Config, Error, HEADER_API_KEY, Result, ids::KEY_PREFIX};

use crate::{Edge, headers};

/// What the 401 says when nothing identified the caller.
pub(crate) const UNAUTHENTICATED: &str =
    "This instance URL requires authentication. Send X-Warren-Key or open a signed URL.";

/// What the 401 says when a session cookie arrives on a cross-origin request.
pub(crate) const CROSS_ORIGIN: &str =
    "The session cookie is only accepted on same-origin requests. Send X-Warren-Key instead.";

/// A successful authentication.
pub(crate) enum Outcome {
    /// Wake the instance and proxy.
    Allowed,
    /// A signed URL was opened: answer with this 302 that sets the session
    /// cookie and points at the same URL without the token.
    Redirect(Response),
}

/// Resolve the caller in contract order: key header, signed URL, cookie.
pub(crate) async fn authorize(
    edge: &Edge,
    method: &Method,
    headers: &HeaderMap,
    uri: &Uri,
    host: &str,
    id: &str,
    host_port: u16,
) -> Result<Outcome> {
    let token_in_query = headers::split_token(uri.query());
    let cookie = headers::session_cookie(headers);
    let has_session = token_in_query.is_some() || cookie.is_some();

    if let Some(key) = key_from_headers(headers, has_session) {
        let (ws, _) = edge.auth.authenticate(&key).await?;
        // The instance must belong to the key's workspace; anything else is a 404.
        edge.lifecycle.get(&ws.id, id).await?;
        tracing::trace!(instance = id, workspace = %ws.id, "instance request authenticated by API key");
        return Ok(Outcome::Allowed);
    }
    if let Some((token, rest)) = token_in_query {
        verify_token(edge, &token, id, host_port)?;
        tracing::debug!(instance = id, "signed URL accepted, setting session cookie");
        return Ok(Outcome::Redirect(redirect_without_token(
            &edge.config,
            host,
            uri.path(),
            &rest,
            &token,
        )?));
    }
    if let Some(token) = cookie {
        verify_token(edge, &token, id, host_port)?;
        let own = own_origins(&edge.config, host, id, host_port);
        if !same_origin(method, headers, &own) {
            tracing::debug!(instance = id, origin = ?headers.get(ORIGIN), referer = ?headers.get(REFERER), site = ?headers.get(SEC_FETCH_SITE), "session cookie refused on a cross-origin request");
            return Err(Error::invalid_api_key(CROSS_ORIGIN));
        }
        tracing::trace!(
            instance = id,
            "instance request authenticated by session cookie"
        );
        return Ok(Outcome::Allowed);
    }
    Err(Error::invalid_api_key(UNAUTHENTICATED))
}

/// The API key to authenticate with, if the headers carry one. `X-Warren-Key`
/// always wins. `Authorization: Bearer` counts when it looks like a Warren key
/// or when there is no signed-URL session to fall back on; a foreign bearer
/// token under an existing session is left alone for the app behind the
/// instance, which receives `Authorization` untouched.
fn key_from_headers(headers: &HeaderMap, has_session: bool) -> Option<String> {
    if let Some(key) = headers
        .get(HEADER_API_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return Some(key.to_string());
    }
    let token = headers::bearer_token(headers)?;
    (token.starts_with(KEY_PREFIX) || !has_session).then(|| token.to_string())
}

const SEC_FETCH_SITE: http::HeaderName = http::HeaderName::from_static("sec-fetch-site");
const SEC_FETCH_MODE: http::HeaderName = http::HeaderName::from_static("sec-fetch-mode");
const SEC_FETCH_DEST: http::HeaderName = http::HeaderName::from_static("sec-fetch-dest");

/// The origins a session cookie may be used from: the instance's own URLs for
/// this host plus `scheme://<Host>` as the client sent it (a browser never
/// lets a page choose the Host of a cross-origin request). Lowercase.
fn own_origins(cfg: &Config, host: &str, id: &str, host_port: u16) -> Vec<String> {
    let mut own = vec![cfg.instance_url(id).to_ascii_lowercase()];
    if host_port != 0 {
        own.push(cfg.preview_url(id, host_port).to_ascii_lowercase());
    }
    if !host.is_empty() {
        own.push(format!("{}://{}", cfg.public_scheme, host).to_ascii_lowercase());
    }
    own
}

/// Whether a cookie-authenticated request is same-origin. A top-level
/// document navigation with a safe method is always accepted: that is what
/// `SameSite=Lax` already permits and how a signed URL opened from the
/// dashboard becomes a cookie request after the 302. Everything else must
/// carry an `Origin`/`Referer` of the instance itself (when present) and a
/// `Sec-Fetch-Site` of `same-origin` or `none` (when present).
pub(crate) fn same_origin(method: &Method, headers: &HeaderMap, own: &[String]) -> bool {
    let header = |name: &http::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let safe = *method == Method::GET || *method == Method::HEAD;
    let navigation = header(&SEC_FETCH_MODE).is_some_and(|m| m.eq_ignore_ascii_case("navigate"))
        && header(&SEC_FETCH_DEST).is_some_and(|d| d.eq_ignore_ascii_case("document"));
    if safe && navigation {
        return true;
    }
    let listed = |origin: &str| {
        let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        own.contains(&origin)
    };
    if let Some(origin) = header(&ORIGIN)
        && !listed(origin)
    {
        return false;
    }
    if let Some(referer) = header(&REFERER)
        && !listed(&origin_of(referer))
    {
        return false;
    }
    if let Some(site) = header(&SEC_FETCH_SITE)
        && !(site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none"))
    {
        return false;
    }
    true
}

/// `scheme://authority` of a URL, or the input when it has no path.
fn origin_of(url: &str) -> String {
    let url = url.trim();
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    format!("{}://{}", &url[..scheme_end], &rest[..end])
}

/// A signed-URL token is good for exactly one instance and one port; port 0
/// in the token is the default-port host.
fn verify_token(edge: &Edge, token: &str, id: &str, host_port: u16) -> Result<()> {
    let (token_id, token_port) = edge.signer.verify(token).map_err(|e| {
        tracing::debug!(instance = id, err = %e, "signed URL token rejected");
        Error::invalid_api_key("Invalid or expired signed URL.")
    })?;
    if token_id != id || token_port != host_port {
        tracing::debug!(instance = id, host_port, token_instance = %token_id, token_port, "signed URL token is for another instance or port");
        return Err(Error::invalid_api_key(
            "This signed URL is for a different instance or port.",
        ));
    }
    Ok(())
}

/// `302` to `scheme://host/path[?rest]` with the session cookie set. The
/// Location is absolute so a browser can never be sent to another host by a
/// path such as `//evil.example/`, which is refused outright.
fn redirect_without_token(
    cfg: &Config,
    host: &str,
    path: &str,
    rest: &str,
    token: &str,
) -> Result<Response> {
    if path.starts_with("//") || path.starts_with("/\\") {
        return Err(Error::invalid(
            "The request path must not start with a double slash.",
        ));
    }
    let path = if path.is_empty() { "/" } else { path };
    let mut location = format!("{}://{host}{path}", cfg.public_scheme);
    if !rest.is_empty() {
        location.push('?');
        location.push_str(rest);
    }
    let location = HeaderValue::from_str(&location)
        .map_err(|_| Error::invalid("The request host or path is not a valid URL."))?;
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(LOCATION, location);
    if let Ok(cookie) = HeaderValue::from_str(&session_cookie(cfg, token)) {
        h.insert(SET_COOKIE, cookie);
    }
    Ok(resp)
}

/// `warren_session=<token>; HttpOnly; SameSite=Lax; Path=/`, plus `Secure`
/// when the public scheme is https.
pub(crate) fn session_cookie(cfg: &Config, token: &str) -> String {
    let mut cookie = format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Lax; Path=/");
    if cfg.public_scheme.eq_ignore_ascii_case("https") {
        cookie.push_str("; Secure");
    }
    cookie
}
