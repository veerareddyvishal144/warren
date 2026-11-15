//! Instance proxying: authenticate, wake, rewrite headers, stream the request
//! and response bodies, and tunnel protocol upgrades.

use std::{
    error::Error as StdError,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request},
    response::{IntoResponse, Response},
};
use http::{
    HeaderMap, HeaderValue, Method, StatusCode, Uri, Version,
    header::{CONNECTION, COOKIE, HOST, ORIGIN, UPGRADE},
};
use hyper::{body::Incoming, upgrade::OnUpgrade};
use warren_core::{Error, HEADER_API_KEY, Result};

use crate::{
    Edge,
    auth::{self, Outcome},
    cors, headers, tunnel,
};

/// Body of the 502 for a bare instance URL whose template has no default port.
pub(crate) const NO_DEFAULT_PORT: &str = "this template has no default port";

/// Serve one request on an instance host.
pub(crate) async fn handle(
    edge: Arc<Edge>,
    req: Request,
    host: String,
    id: String,
    host_port: u16,
) -> Response {
    if req.method() == Method::OPTIONS {
        return cors::preflight(req.headers());
    }
    let origin = req.headers().get(ORIGIN).cloned();
    let mut resp = match serve(&edge, req, &host, &id, host_port).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(instance = %id, code = %e.code, status = e.status, message = %e.message, "instance request not proxied");
            e.into_response()
        }
    };
    cors::apply(&mut resp, origin.as_ref());
    resp
}

async fn serve(
    edge: &Edge,
    req: Request,
    host: &str,
    id: &str,
    host_port: u16,
) -> Result<Response> {
    if let Outcome::Redirect(resp) =
        auth::authorize(edge, req.headers(), req.uri(), id, host_port).await?
    {
        return Ok(resp);
    }
    let inst = edge.lifecycle.ensure_awake(id).await?;
    edge.lifecycle.record_activity(id);
    let ip = edge.lifecycle.endpoint(id).await?;
    let port = if host_port == 0 {
        inst.default_port
    } else {
        host_port
    };
    if port == 0 {
        return Ok((StatusCode::BAD_GATEWAY, NO_DEFAULT_PORT).into_response());
    }
    forward(edge, req, host, id, &ip, port).await
}

/// Send the request to `ip:port` and relay the answer, tunnelling upgrades.
async fn forward(
    edge: &Edge,
    mut req: Request,
    host: &str,
    id: &str,
    ip: &str,
    port: u16,
) -> Result<Response> {
    // hyper parks the client's pending upgrade in the extensions. Taking it
    // out here (what `hyper::upgrade::on` does) tells us whether this request
    // can be upgraded at all: HTTP/2 and in-process callers have none.
    let client_upgrade = req.extensions_mut().remove::<OnUpgrade>();
    let upgrade = client_upgrade.is_some() && req.headers().contains_key(UPGRADE);
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let authority = headers::authority(ip, port);
    let method = req.method().clone();
    let target = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), |p| p.as_str().to_string());
    let upstream = build_upstream(
        req,
        &authority,
        host,
        &edge.config.public_scheme,
        peer,
        upgrade,
    )?;
    tracing::debug!(instance = id, %method, target = %target, upstream = %authority, upgrade, "proxying");

    let mut resp = match tokio::time::timeout(
        edge.opts.response_timeout,
        edge.client.request(upstream),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::warn!(instance = id, upstream = %authority, err = %error_chain(&e), "upstream request failed");
            return Err(map_client_error(&e));
        }
        Err(_) => {
            tracing::warn!(instance = id, upstream = %authority, timeout = ?edge.opts.response_timeout, "upstream did not answer in time");
            return Err(Error::timeout(format!(
                "The instance did not answer within {} seconds.",
                edge.opts.response_timeout.as_secs()
            )));
        }
    };
    edge.lifecycle.record_activity(id);

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade.filter(|_| upgrade) else {
            return Err(Error::bad_gateway(
                "The instance switched protocols on a request that cannot be upgraded.",
            ));
        };
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(tunnel::run(
            client_upgrade,
            upstream_upgrade,
            edge.lifecycle.clone(),
            id.to_string(),
            edge.opts.activity_heartbeat,
        ));
        return Ok(relay(resp, true));
    }
    Ok(relay(resp, false))
}

/// The upstream request: same method, path and query; `Host` rewritten to the
/// instance; hop-by-hop headers, `X-Warren-Key` and the session cookie
/// removed; `Authorization` kept; `X-Forwarded-*` set. The body streams.
fn build_upstream(
    req: Request,
    authority: &str,
    original_host: &str,
    scheme: &str,
    peer: Option<IpAddr>,
    upgrade: bool,
) -> Result<Request<Body>> {
    let (parts, body) = req.into_parts();
    let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let uri: Uri = format!("http://{authority}{target}")
        .parse()
        .map_err(|_| Error::invalid("Malformed request target."))?;

    let nominated = headers::connection_nominated(&parts.headers);
    let mut out = HeaderMap::with_capacity(parts.headers.len() + 4);
    for (name, value) in &parts.headers {
        let keep_for_upgrade = upgrade && (*name == CONNECTION || *name == UPGRADE);
        let hop = headers::is_hop_by_hop(name) || nominated.iter().any(|n| n == name.as_str());
        let ours = *name == HOST
            || *name == COOKIE
            || name.as_str() == HEADER_API_KEY
            || *name == headers::X_FORWARDED_HOST
            || *name == headers::X_FORWARDED_PROTO
            || *name == headers::X_FORWARDED_FOR;
        if (hop && !keep_for_upgrade) || ours {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out.insert(
        HOST,
        HeaderValue::from_str(authority)
            .map_err(|_| Error::internal("invalid upstream authority"))?,
    );
    if let Some(cookies) = headers::cookies_without_session(&parts.headers) {
        out.insert(COOKIE, cookies);
    }
    if let Some(v) = Some(original_host)
        .filter(|h| !h.is_empty())
        .and_then(|h| HeaderValue::from_str(h).ok())
    {
        out.insert(headers::X_FORWARDED_HOST, v);
    }
    if let Ok(v) = HeaderValue::from_str(scheme) {
        out.insert(headers::X_FORWARDED_PROTO, v);
    }
    if let Some(v) = headers::forwarded_for(&parts.headers, peer) {
        out.insert(headers::X_FORWARDED_FOR, v);
    }

    let mut upstream = Request::builder()
        .method(parts.method)
        .uri(uri)
        .version(Version::HTTP_11)
        .body(body)
        .map_err(Error::internal)?;
    *upstream.headers_mut() = out;
    Ok(upstream)
}

/// The client's response: status and headers minus hop-by-hop ones, the body
/// streamed straight through. A `101` keeps `Connection`/`Upgrade` and has
/// no body; the bytes flow through the tunnel instead.
fn relay(resp: hyper::Response<Incoming>, switching: bool) -> Response {
    let (parts, body) = resp.into_parts();
    let nominated = headers::connection_nominated(&parts.headers);
    let mut out = Response::new(if switching {
        Body::empty()
    } else {
        Body::new(body)
    });
    *out.status_mut() = parts.status;
    for (name, value) in &parts.headers {
        let keep_for_upgrade = switching && (*name == CONNECTION || *name == UPGRADE);
        let hop = headers::is_hop_by_hop(name) || nominated.iter().any(|n| n == name.as_str());
        if hop && !keep_for_upgrade {
            continue;
        }
        out.headers_mut().append(name.clone(), value.clone());
    }
    out
}

/// Connection failures are `502 bad_gateway`; a connect that timed out is
/// `504 timeout`.
fn map_client_error(e: &hyper_util::client::legacy::Error) -> Error {
    let chain = error_chain(e);
    if timed_out(e) {
        Error::timeout(format!(
            "The instance did not accept the connection in time: {chain}"
        ))
    } else if e.is_connect() {
        Error::bad_gateway(format!("Could not connect to the instance: {chain}"))
    } else {
        Error::bad_gateway(format!(
            "The instance did not return a valid response: {chain}"
        ))
    }
}

/// Whether an `io::Error` of kind `TimedOut` is anywhere in the cause chain.
fn timed_out(e: &(dyn StdError + 'static)) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(e);
    while let Some(err) = current {
        if err
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::TimedOut)
        {
            return true;
        }
        current = err.source();
    }
    false
}

/// The error and every cause, colon-separated, for logs and 502 bodies.
fn error_chain(e: &(dyn StdError + 'static)) -> String {
    let mut text = e.to_string();
    let mut current = e.source();
    while let Some(err) = current {
        text.push_str(": ");
        text.push_str(&err.to_string());
        current = err.source();
    }
    text
}
