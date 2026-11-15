//! Who may reach an instance URL: an API key in `X-Warren-Key` or
//! `Authorization: Bearer`, a signed URL (`?warren_token=`) that becomes the
//! `warren_session` cookie, or that cookie on later requests.

use axum::{body::Body, response::Response};
use http::{
    HeaderMap, HeaderValue, StatusCode, Uri,
    header::{AUTHORIZATION, LOCATION, SET_COOKIE},
};
use warren_core::{COOKIE_NAME, Config, Error, HEADER_API_KEY, Result, ids::KEY_PREFIX};

use crate::{Edge, headers};

/// What the 401 says when nothing identified the caller.
pub(crate) const UNAUTHENTICATED: &str =
    "This instance URL requires authentication. Send X-Warren-Key or open a signed URL.";

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
    headers: &HeaderMap,
    uri: &Uri,
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
            uri.path(),
            &rest,
            &token,
        )));
    }
    if let Some(token) = cookie {
        verify_token(edge, &token, id, host_port)?;
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
    let auth = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let scheme = auth.get(..7)?;
    if !scheme.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    let token = auth[7..].trim();
    (token.starts_with(KEY_PREFIX) || !has_session).then(|| token.to_string())
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

/// `302` to `path[?rest]` with the session cookie set.
fn redirect_without_token(cfg: &Config, path: &str, rest: &str, token: &str) -> Response {
    let location = if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{rest}")
    };
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(
        LOCATION,
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    if let Ok(cookie) = HeaderValue::from_str(&session_cookie(cfg, token)) {
        h.insert(SET_COOKIE, cookie);
    }
    resp
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
