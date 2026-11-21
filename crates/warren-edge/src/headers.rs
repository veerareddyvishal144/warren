//! Header surgery shared by the proxy: hop-by-hop filtering, forwarding
//! headers, the session cookie and the signed-URL query parameter. Pure
//! functions, unit-tested on their own.

use std::net::IpAddr;

use http::{
    HeaderMap, HeaderValue,
    header::{AUTHORIZATION, CONNECTION, COOKIE, HeaderName},
};
use warren_core::{COOKIE_NAME, QUERY_TOKEN, ids::KEY_PREFIX};

pub(crate) const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
pub(crate) const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
pub(crate) const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Connection-level headers a proxy must not forward (RFC 9110 section 7.6.1).
pub(crate) fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Lowercase header names listed in `Connection: a, b`; they are hop-by-hop too.
pub(crate) fn connection_nominated(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The token of an `Authorization: Bearer <token>` header, trimmed.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let auth = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let scheme = auth.get(..7)?;
    if !scheme.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    Some(auth[7..].trim())
}

/// Whether `Authorization` carries a Warren API key (`Bearer sk_live_...`).
/// Such a header is the edge's credential and is never forwarded.
pub(crate) fn is_warren_bearer(headers: &HeaderMap) -> bool {
    bearer_token(headers).is_some_and(|t| t.starts_with(KEY_PREFIX))
}

/// `(name, "name=value")` for every cookie in every `Cookie` header.
fn cookie_pairs(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            (
                pair.split_once('=').map_or(pair, |(name, _)| name.trim()),
                pair,
            )
        })
}

/// Value of the `warren_session` cookie, if any.
pub(crate) fn session_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_pairs(headers)
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, pair)| {
            pair.split_once('=')
                .map_or("", |(_, value)| value.trim())
                .to_string()
        })
}

/// The `Cookie` header to forward: every cookie except `warren_session`, or
/// `None` when nothing is left.
pub(crate) fn cookies_without_session(headers: &HeaderMap) -> Option<HeaderValue> {
    let kept: Vec<&str> = cookie_pairs(headers)
        .filter(|(name, _)| *name != COOKIE_NAME)
        .map(|(_, pair)| pair)
        .collect();
    if kept.is_empty() {
        None
    } else {
        HeaderValue::from_str(&kept.join("; ")).ok()
    }
}

/// Split `warren_token` out of a raw query string. Returns the token and the
/// remaining query (possibly empty) with every other parameter untouched.
pub(crate) fn split_token(query: Option<&str>) -> Option<(String, String)> {
    let mut token: Option<String> = None;
    let mut rest = Vec::new();
    for pair in query?.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == QUERY_TOKEN {
            token.get_or_insert_with(|| value.to_string());
        } else if !pair.is_empty() {
            rest.push(pair);
        }
    }
    token.map(|t| (t, rest.join("&")))
}

/// `X-Forwarded-For` for the upstream: the chain the client sent plus the
/// peer address when the listener provides it.
pub(crate) fn forwarded_for(headers: &HeaderMap, peer: Option<IpAddr>) -> Option<HeaderValue> {
    let mut chain: Vec<String> = headers
        .get_all(X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    if let Some(ip) = peer {
        chain.push(ip.to_string());
    }
    if chain.is_empty() {
        None
    } else {
        HeaderValue::from_str(&chain.join(", ")).ok()
    }
}
