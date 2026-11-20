//! The forward-proxy half: `CONNECT host:port` tunnels and absolute-form
//! plain HTTP requests (`GET http://host/path HTTP/1.1`), both only for
//! `allowlist` instances, only to hosts on their allowlist and only to public
//! addresses. The host name is resolved here, once, and the connection goes
//! to the resolved address, so a name that resolves to a private range is
//! refused instead of reached.

use std::net::{IpAddr, SocketAddr};

use http::{
    HeaderMap, Request, Response, StatusCode, Uri,
    header::{self, HeaderName},
};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use warren_core::{BrokerEntry, Error, Result, broker};

use crate::server::{Body, Broker, Reply, check_proxy_mode, empty};

/// Hop-by-hop headers (RFC 9110 section 7.6.1) plus the proxy ones; never forwarded.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub(crate) fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// Headers the `Connection` header nominates as hop-by-hop.
fn nominated(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Copy `from` into a new map without hop-by-hop headers, nominated headers
/// and `Host` (the caller sets its own).
pub(crate) fn end_to_end_headers(from: &HeaderMap) -> HeaderMap {
    let nominated = nominated(from);
    let mut out = HeaderMap::with_capacity(from.len());
    for (name, value) in from {
        if is_hop_by_hop(name)
            || *name == header::HOST
            || nominated.iter().any(|n| n == name.as_str())
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Split `host[:port]` (brackets allowed around an IPv6 literal).
fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return Err(Error::invalid("request names no host"));
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err(Error::invalid("malformed IPv6 host"));
        };
        let host = &rest[..end];
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| Error::invalid("malformed port"))?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            let port = port
                .parse::<u16>()
                .map_err(|_| Error::invalid("malformed port"))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Policy for one destination: the instance proxies, the host is on its
/// allowlist and it resolves to at least one public address. Returns the
/// addresses to try, in resolver order.
async fn admit(broker: &Broker, entry: &BrokerEntry, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    check_proxy_mode(entry)?;
    if port == 0 {
        return Err(Error::invalid("port 0 is not a destination"));
    }
    if !broker::host_matches(host, &entry.allowed_domains) {
        return Err(Error::forbidden(format!(
            "{host} is not on this instance's allowed_domains"
        )));
    }
    resolve(broker, host, port).await
}

/// Resolve `host:port`, dropping addresses the broker never connects to.
pub(crate) async fn resolve(broker: &Broker, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| Error::bad_gateway(format!("cannot resolve {host}: {e}")))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(Error::bad_gateway(format!("{host} has no address")));
    }
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| broker.opts.allow_forbidden_destinations || !broker::is_forbidden_ip(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(Error::forbidden(format!(
            "{host} resolves only to private, loopback or reserved addresses"
        )));
    }
    Ok(allowed)
}

/// Connect to the first address that accepts, within the connect timeout.
pub(crate) async fn dial(broker: &Broker, addrs: &[SocketAddr], what: &str) -> Result<TcpStream> {
    let mut last = None;
    for addr in addrs {
        match tokio::time::timeout(broker.opts.connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(e)) => last = Some(format!("{addr}: {e}")),
            Err(_) => last = Some(format!("{addr}: connect timed out")),
        }
    }
    Err(Error::bad_gateway(format!(
        "cannot connect to {what}: {}",
        last.unwrap_or_default()
    )))
}

/// `CONNECT host:port`: open the tunnel and splice the upgraded connection
/// into it once hyper hands it over.
pub(crate) async fn connect(
    broker: &Broker,
    entry: &BrokerEntry,
    req: Request<Incoming>,
) -> Result<Reply> {
    let authority = req
        .uri()
        .authority()
        .map(|a| a.to_string())
        .ok_or_else(|| Error::invalid("CONNECT needs host:port"))?;
    let (host, port) = split_authority(&authority, 443)?;
    let addrs = admit(broker, entry, &host, port).await?;
    let upstream = dial(broker, &addrs, &format!("{host}:{port}")).await?;
    tracing::debug!(instance = %entry.instance_id, source = %entry.ip, %host, port, "CONNECT allowed");
    let instance = entry.instance_id.clone();
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let mut client = TokioIo::new(upgraded);
                let mut upstream = upstream;
                match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                    Ok((up, down)) => {
                        tracing::debug!(instance = %instance, %host, port, up, down, "tunnel closed")
                    }
                    Err(e) => tracing::debug!(instance = %instance, %host, port, error = %e, "tunnel ended"),
                }
            }
            Err(e) => tracing::debug!(instance = %instance, error = %e, "CONNECT upgrade failed"),
        }
    });
    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(empty())
        .expect("static response"))
}

/// An absolute-form request (`GET http://host/path`): send it to the host
/// over a fresh connection to the resolved address and relay the answer,
/// body streaming both ways.
pub(crate) async fn forward(
    broker: &Broker,
    entry: &BrokerEntry,
    req: Request<Incoming>,
) -> Result<Reply> {
    let uri = req.uri().clone();
    if uri.scheme_str() != Some("http") {
        return Err(Error::invalid(
            "only http:// requests and CONNECT tunnels are proxied",
        ));
    }
    let authority = uri
        .authority()
        .map(|a| a.to_string())
        .ok_or_else(|| Error::invalid("request names no host"))?;
    let (host, port) = split_authority(&authority, 80)?;
    let addrs = admit(broker, entry, &host, port).await?;
    let stream = dial(broker, &addrs, &authority).await?;
    tracing::debug!(instance = %entry.instance_id, source = %entry.ip, method = %req.method(), %host, port, path = uri.path(), "HTTP allowed");

    let (parts, body) = req.into_parts();
    let target: Uri = parts
        .uri
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .parse()
        .map_err(|_| Error::invalid("malformed request target"))?;
    let mut headers = end_to_end_headers(&parts.headers);
    headers.insert(
        header::HOST,
        authority
            .parse()
            .map_err(|_| Error::invalid("malformed host"))?,
    );
    let mut out = Request::builder()
        .method(parts.method)
        .uri(target)
        .version(http::Version::HTTP_11)
        .body(body)
        .map_err(|e| Error::invalid(format!("malformed request: {e}")))?;
    *out.headers_mut() = headers;

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| Error::bad_gateway(format!("{authority}: {e}")))?;
    let instance = entry.instance_id.clone();
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(instance = %instance, error = %e, "upstream connection ended with an error");
        }
    });
    let resp = tokio::time::timeout(broker.opts.response_timeout, sender.send_request(out))
        .await
        .map_err(|_| Error::timeout(format!("{authority} did not answer in time")))?
        .map_err(|e| Error::bad_gateway(format!("{authority}: {e}")))?;
    Ok(relay(resp))
}

/// The upstream answer without hop-by-hop headers, body streaming.
fn relay(resp: Response<Incoming>) -> Reply {
    let (parts, body) = resp.into_parts();
    let mut out = Response::new(body.map_err(|e| Box::new(e) as _).boxed() as Body);
    *out.status_mut() = parts.status;
    *out.version_mut() = http::Version::HTTP_11;
    *out.headers_mut() = end_to_end_headers(&parts.headers);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities_split() {
        assert_eq!(
            split_authority("api.example.com:443", 80).unwrap(),
            ("api.example.com".into(), 443)
        );
        assert_eq!(
            split_authority("api.example.com", 80).unwrap(),
            ("api.example.com".into(), 80)
        );
        assert_eq!(
            split_authority("[2001:db8::1]:8443", 80).unwrap(),
            ("2001:db8::1".into(), 8443)
        );
        assert_eq!(
            split_authority("[2001:db8::1]", 443).unwrap(),
            ("2001:db8::1".into(), 443)
        );
        assert!(split_authority("", 80).is_err());
        assert!(split_authority("host:notaport", 80).is_err());
        assert!(split_authority("[2001:db8::1", 80).is_err());
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "a".parse().unwrap());
        h.insert(header::CONNECTION, "keep-alive, x-custom".parse().unwrap());
        h.insert("x-custom", "1".parse().unwrap());
        h.insert("proxy-connection", "keep-alive".parse().unwrap());
        h.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        h.insert(header::ACCEPT, "*/*".parse().unwrap());
        h.insert("x-api-key", "k".parse().unwrap());
        let out = end_to_end_headers(&h);
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["accept", "x-api-key"]);
    }
}
