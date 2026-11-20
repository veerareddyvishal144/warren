//! Provider routes: `/<r>/<token>/<rest>` from a registered address becomes a
//! request to the secret's upstream with the credential header added. The
//! upstream client resolves names through a resolver that drops private and
//! reserved addresses, never follows redirects (a redirect could point
//! inside) and streams the answer back.

use std::{net::IpAddr, sync::Arc};

use bytes::Bytes;
use futures::TryStreamExt;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response, header};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use warren_core::{BrokerEntry, BrokerRoute, Error, Result, broker};

use crate::{
    Options,
    proxy::end_to_end_headers,
    server::{Body, Broker, Reply},
};

/// Longest upstream URL built from a route (the upstream plus the request path).
const MAX_URL_BYTES: usize = 8192;

/// A resolver for the upstream client that refuses private, loopback and
/// reserved answers (and everything when `allow_forbidden` is off and nothing
/// public remains).
struct GuardedResolver {
    allow_forbidden: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow_forbidden;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| allow || !broker::is_forbidden_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!(
                    "{host} resolves only to private, loopback or reserved addresses"
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The reqwest client routes use: guarded DNS, no redirects, no system
/// proxy, bounded connect time, HTTP/1 only (upstreams may be anything).
pub(crate) fn client(opts: &Options) -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(GuardedResolver {
            allow_forbidden: opts.allow_forbidden_destinations,
        }))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(opts.connect_timeout)
        .user_agent(concat!("warren-broker/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client with static options")
}

/// `/r/<token>/rest` -> (token, "/rest" or "").
fn split_route_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(broker::ROUTE_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    match rest.find('/') {
        Some(i) => Some((&rest[..i], &rest[i..])),
        None => Some((rest, "")),
    }
}

/// The upstream URL: the secret's upstream (scheme, host, port, path prefix)
/// with the request's remaining path appended and its query kept.
fn upstream_url(route: &BrokerRoute, rest: &str, query: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&route.upstream)
        .map_err(|e| Error::internal(format!("route upstream {:?}: {e}", route.upstream)))?;
    let base = url.path().trim_end_matches('/').to_string();
    let path = if rest.is_empty() {
        if base.is_empty() {
            "/".to_string()
        } else {
            base
        }
    } else {
        format!("{base}{rest}")
    };
    url.set_path(&path);
    url.set_query(query);
    if url.as_str().len() > MAX_URL_BYTES {
        return Err(Error::invalid("request path is too long"));
    }
    Ok(url)
}

/// Refuse an upstream whose host is a literal address in a forbidden range
/// (names go through the guarded resolver instead).
fn check_literal_host(broker: &Broker, url: &reqwest::Url) -> Result<()> {
    if broker.opts.allow_forbidden_destinations {
        return Ok(());
    }
    let host = url.host_str().unwrap_or("");
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>();
    if let Ok(ip) = literal
        && broker::is_forbidden_ip(ip)
    {
        return Err(Error::forbidden(format!(
            "upstream {host} is a private, loopback or reserved address"
        )));
    }
    Ok(())
}

/// Serve one route request for `entry`.
pub(crate) async fn handle(
    broker: &Broker,
    entry: &BrokerEntry,
    req: Request<Incoming>,
) -> Result<Reply> {
    let Some((token, rest)) = split_route_path(req.uri().path()) else {
        return Err(Error::not_found("route"));
    };
    let Some(route) = entry.routes.iter().find(|r| r.token == token) else {
        // Tokens are bound to the registered address: a token of another
        // instance is simply unknown here.
        return Err(Error::not_found("route"));
    };
    let url = upstream_url(route, rest, req.uri().query())?;
    check_literal_host(broker, &url)?;
    tracing::debug!(
        instance = %entry.instance_id,
        source = %entry.ip,
        method = %req.method(),
        upstream = %url.host_str().unwrap_or(""),
        path = url.path(),
        "route allowed"
    );

    let (parts, body) = req.into_parts();
    let header_name = HeaderName::from_bytes(route.header.as_bytes())
        .map_err(|_| Error::internal(format!("route header {:?} is invalid", route.header)))?;
    let mut headers = end_to_end_headers(&parts.headers);
    headers.remove(&header_name);
    headers.remove(header::CONTENT_LENGTH);
    let value = HeaderValue::from_str(&broker::header_value(&route.header, &route.value))
        .map_err(|_| Error::internal("route value is not a valid header value"))?;
    headers.insert(header_name, value);

    let body = Limited::new(body, broker.opts.max_route_body_bytes)
        .collect()
        .await
        .map_err(|e| {
            if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() {
                Error::invalid(format!(
                    "request body exceeds {} bytes",
                    broker.opts.max_route_body_bytes
                ))
            } else {
                Error::invalid(format!("reading the request body: {e}"))
            }
        })?
        .to_bytes();

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|_| Error::invalid("unsupported method"))?;
    let upstream_host = url.host_str().unwrap_or("").to_string();
    let request = broker
        .http
        .request(method, url)
        .headers(headers)
        .body(body);
    let resp = tokio::time::timeout(broker.opts.response_timeout, request.send())
        .await
        .map_err(|_| Error::timeout(format!("{upstream_host} did not answer in time")))?
        .map_err(|e| {
            if e.is_connect() {
                Error::bad_gateway(format!("cannot connect to {upstream_host}: {e}"))
            } else {
                Error::bad_gateway(format!("{upstream_host}: {e}"))
            }
        })?;
    Ok(relay(resp))
}

/// The upstream answer: status and end-to-end headers, body streamed.
fn relay(resp: reqwest::Response) -> Reply {
    let status = resp.status();
    let headers: HeaderMap = end_to_end_headers(resp.headers());
    let stream = resp
        .bytes_stream()
        .map_ok(|chunk: Bytes| Frame::data(chunk))
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
    let body: Body = StreamBody::new(stream).boxed();
    let mut out = Response::new(body);
    *out.status_mut() = http::StatusCode::from_u16(status.as_u16())
        .unwrap_or(http::StatusCode::BAD_GATEWAY);
    *out.headers_mut() = headers;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(upstream: &str) -> BrokerRoute {
        BrokerRoute {
            token: "t".into(),
            upstream: upstream.into(),
            header: "x-api-key".into(),
            value: "v".into(),
        }
    }

    #[test]
    fn route_paths_split() {
        assert_eq!(split_route_path("/r/abc/v1/x"), Some(("abc", "/v1/x")));
        assert_eq!(split_route_path("/r/abc"), Some(("abc", "")));
        assert_eq!(split_route_path("/r/abc/"), Some(("abc", "/")));
        assert_eq!(split_route_path("/r/"), None);
        assert_eq!(split_route_path("/x/abc"), None);
    }

    #[test]
    fn upstream_urls_keep_prefix_and_query() {
        let r = route("https://api.example.com");
        assert_eq!(
            upstream_url(&r, "/v1/messages", Some("a=1")).unwrap().as_str(),
            "https://api.example.com/v1/messages?a=1"
        );
        assert_eq!(
            upstream_url(&r, "", None).unwrap().as_str(),
            "https://api.example.com/"
        );
        let r = route("https://api.example.com/base/");
        assert_eq!(
            upstream_url(&r, "/v1", None).unwrap().as_str(),
            "https://api.example.com/base/v1"
        );
        assert_eq!(
            upstream_url(&r, "", None).unwrap().as_str(),
            "https://api.example.com/base"
        );
        let r = route("https://host.example.com:8443/p");
        assert_eq!(
            upstream_url(&r, "/q", None).unwrap().as_str(),
            "https://host.example.com:8443/p/q"
        );
        let long = format!("/{}", "x".repeat(MAX_URL_BYTES));
        assert!(upstream_url(&r, &long, None).is_err());
    }
}
