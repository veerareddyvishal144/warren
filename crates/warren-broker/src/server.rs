//! The listener, the per-connection service and the shared answer helpers.

use std::{convert::Infallible, net::IpAddr, sync::Arc};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use warren_core::{BrokerEntry, Error, NetworkMode, Result, broker::ROUTE_PREFIX};

use crate::{Options, Registry, proxy, routes};

/// Error type of every body the broker returns.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// Every response body the broker produces.
pub(crate) type Body = BoxBody<Bytes, BoxError>;
pub(crate) type Reply = Response<Body>;

/// The broker: policy, the upstream HTTP client for routes and the registry.
pub struct Broker {
    pub(crate) registry: Arc<Registry>,
    pub(crate) opts: Options,
    pub(crate) http: reqwest::Client,
}

impl Broker {
    pub fn new(registry: Arc<Registry>, opts: Options) -> Broker {
        let http = routes::client(&opts);
        Broker {
            registry,
            opts,
            http,
        }
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Answer one request from `peer`. Never fails: every problem is a JSON
    /// error answer, so the connection stays usable.
    pub async fn handle(
        self: Arc<Self>,
        peer: IpAddr,
        req: Request<Incoming>,
    ) -> std::result::Result<Reply, Infallible> {
        let method = req.method().clone();
        let target = req.uri().to_string();
        let reply = match self.dispatch(peer, req).await {
            Ok(reply) => reply,
            Err(e) => {
                tracing::debug!(%peer, %method, %target, code = %e.code, message = %e.message, "refused");
                error_reply(&e)
            }
        };
        Ok(reply)
    }

    async fn dispatch(&self, peer: IpAddr, req: Request<Incoming>) -> Result<Reply> {
        if req.method() == Method::GET && req.uri().path() == "/healthz" {
            return Ok(text(StatusCode::OK, "warren-broker ok\n"));
        }
        let Some(entry) = self.registry.lookup(peer) else {
            return Err(Error::forbidden(format!(
                "{peer} is not the address of a running instance this broker knows"
            )));
        };
        if req.method() == Method::CONNECT {
            return proxy::connect(self, &entry, req).await;
        }
        if req.uri().scheme().is_some() {
            return proxy::forward(self, &entry, req).await;
        }
        if req.uri().path().starts_with(ROUTE_PREFIX) {
            return routes::handle(self, &entry, req).await;
        }
        Err(Error::not_found("route"))
    }
}

/// Refuse proxying for modes that do not use the broker as a proxy.
pub(crate) fn check_proxy_mode(entry: &BrokerEntry) -> Result<()> {
    match entry.mode {
        NetworkMode::Allowlist => Ok(()),
        NetworkMode::DenyAll => Err(Error::forbidden(
            "this instance has network deny-all; nothing is proxied",
        )),
        NetworkMode::AllowAll => Err(Error::forbidden(
            "this instance has network allow-all and does not proxy through the broker",
        )),
    }
}

pub(crate) fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

fn text(status: StatusCode, body: &'static str) -> Reply {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(body))
        .expect("static response")
}

/// `{ "error": { "code", "message" } }` with the error's status, like the API.
pub(crate) fn error_reply(e: &Error) -> Reply {
    let public = e.public();
    let body = serde_json::json!({ "error": { "code": public.code, "message": public.message } });
    Response::builder()
        .status(StatusCode::from_u16(public.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONNECTION, "close")
        .body(full(body.to_string()))
        .expect("error response")
}

/// Accept connections on `listener` until `shutdown` cancels and serve each
/// with HTTP/1 (the only version proxy clients speak), upgrades enabled for
/// `CONNECT`. Open connections finish on their own after the loop ends.
pub async fn serve(
    listener: TcpListener,
    broker: Arc<Broker>,
    shutdown: CancellationToken,
) -> Result<()> {
    let addr = listener
        .local_addr()
        .map_err(|e| Error::internal(format!("broker listener: {e}")))?;
    tracing::info!(%addr, "broker listening");
    loop {
        let (stream, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "broker accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        let broker = broker.clone();
        tokio::spawn(async move {
            let ip = peer.ip();
            let service = service_fn(move |req| broker.clone().handle(ip, req));
            let conn = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            if let Err(e) = conn.await {
                tracing::debug!(%peer, error = %e, "broker connection ended with an error");
            }
        });
    }
    tracing::info!("broker stopped accepting connections");
    Ok(())
}
