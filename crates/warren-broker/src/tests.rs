//! End-to-end tests over loopback: a broker, a recording upstream and raw
//! TCP or reqwest clients. The test client's source address is 127.0.0.1, so
//! that is the address registered for the "instance".

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use warren_core::{BrokerEntry, BrokerRegistry, BrokerRoute, NetworkMode};

use crate::{Broker, Options, Registry, serve};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// One request as the upstream saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A plain HTTP server that records requests and answers `200 {"ok":true}`
/// with the path echoed in a header.
struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    async fn start() -> Upstream {
        let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let recorder = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let recorder = recorder.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            let path = parts
                                .uri
                                .path_and_query()
                                .map(|p| p.as_str().to_string())
                                .unwrap_or_default();
                            recorder.lock().push(Seen {
                                method: parts.method.to_string(),
                                path: path.clone(),
                                headers: parts
                                    .headers
                                    .iter()
                                    .map(|(k, v)| {
                                        (k.to_string(), v.to_str().unwrap_or("").to_string())
                                    })
                                    .collect(),
                                body: String::from_utf8_lossy(&body).into_owned(),
                            });
                            let resp = Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .header("x-echo-path", path)
                                .header("connection", "close")
                                .header("x-upstream", "yes")
                                .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                                .unwrap();
                            Ok::<_, std::convert::Infallible>(resp)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Upstream { addr, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

struct Harness {
    addr: SocketAddr,
    registry: Arc<Registry>,
    _shutdown: CancellationToken,
}

impl Harness {
    async fn start(allow_forbidden: bool) -> Harness {
        let registry = Registry::new();
        let broker = Arc::new(Broker::new(
            registry.clone(),
            Options {
                allow_forbidden_destinations: allow_forbidden,
                ..Options::default()
            },
        ));
        let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(serve(listener, broker, shutdown.clone()));
        Harness {
            addr,
            registry,
            _shutdown: shutdown,
        }
    }

    fn register(&self, mode: NetworkMode, domains: &[&str], routes: Vec<BrokerRoute>) {
        self.registry.register(BrokerEntry {
            instance_id: "ab12cd34ef".into(),
            ip: LOCAL,
            mode,
            allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
            routes,
        });
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Send raw bytes and read until the peer closes or `stop` appears.
    async fn raw(&self, request: &str) -> String {
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }
}

fn route(token: &str, upstream: &str, header: &str, value: &str) -> BrokerRoute {
    BrokerRoute {
        token: token.into(),
        upstream: upstream.into(),
        header: header.into(),
        value: value.into(),
    }
}

async fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await.unwrap();
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn unknown_source_is_refused_but_health_answers() {
    let h = Harness::start(true).await;
    let resp = reqwest::get(h.url("/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = reqwest::get(h.url("/r/sometoken/x")).await.unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden");
    let head = h
        .raw("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
}

#[tokio::test]
async fn connect_is_allowed_only_for_listed_hosts_in_allowlist_mode() {
    let up = Upstream::start().await;
    let h = Harness::start(true).await;
    let dest = format!("127.0.0.1:{}", up.addr.port());
    h.register(NetworkMode::Allowlist, &["127.0.0.1"], vec![]);

    // Allowed: the tunnel carries a request to the upstream.
    let mut s = TcpStream::connect(h.addr).await.unwrap();
    s.write_all(format!("CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let head = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    s.write_all(b"GET /tunnelled?x=1 HTTP/1.1\r\nHost: upstream\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("{\"ok\":true}"));
    assert_eq!(up.seen()[0].path, "/tunnelled?x=1");

    // Denied: host not on the allowlist.
    let head = h
        .raw("CONNECT evil.example.net:443 HTTP/1.1\r\nHost: evil.example.net:443\r\n\r\n")
        .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains("allowed_domains"));

    // Denied: deny-all and allow-all never proxy.
    for mode in [NetworkMode::DenyAll, NetworkMode::AllowAll] {
        h.register(mode, &["127.0.0.1"], vec![]);
        let head = h
            .raw(&format!(
                "CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\n\r\n"
            ))
            .await;
        assert!(head.starts_with("HTTP/1.1 403"), "{mode}: {head}");
    }
    assert_eq!(up.seen().len(), 1);
}

#[tokio::test]
async fn private_destinations_are_blocked_even_when_listed() {
    let up = Upstream::start().await;
    let h = Harness::start(false).await;
    let dest = format!("127.0.0.1:{}", up.addr.port());
    h.register(
        NetworkMode::Allowlist,
        &["127.0.0.1", "localhost", "metadata.internal"],
        vec![route("tok", &format!("https://127.0.0.1:{}", up.addr.port()), "x-api-key", "v")],
    );
    let head = h
        .raw(&format!("CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\n\r\n"))
        .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    assert!(head.contains("private, loopback or reserved"));
    let head = h
        .raw(&format!(
            "GET http://{dest}/x HTTP/1.1\r\nHost: {dest}\r\n\r\n"
        ))
        .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    // A route whose upstream is a loopback literal is refused too.
    let resp = reqwest::get(h.url("/r/tok/v1")).await.unwrap();
    assert_eq!(resp.status(), 403);
    assert!(up.seen().is_empty(), "nothing reached the upstream");
}

#[tokio::test]
async fn plain_http_is_forwarded_with_path_and_headers() {
    let up = Upstream::start().await;
    let h = Harness::start(true).await;
    let dest = format!("127.0.0.1:{}", up.addr.port());
    h.register(NetworkMode::Allowlist, &["127.0.0.1"], vec![]);
    let out = h
        .raw(&format!(
            "POST http://{dest}/api/v1?q=2 HTTP/1.1\r\nHost: {dest}\r\nProxy-Connection: keep-alive\r\nX-Custom: yes\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
        ))
        .await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("x-upstream: yes") || out.contains("X-Upstream: yes"), "{out}");
    assert!(out.contains("{\"ok\":true}"));
    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/api/v1?q=2");
    assert_eq!(seen[0].body, "hello");
    assert_eq!(seen[0].header("host"), Some(dest.as_str()));
    assert_eq!(seen[0].header("x-custom"), Some("yes"));
    assert!(seen[0].header("proxy-connection").is_none());

    // https:// in absolute form is not a thing; and unlisted hosts are refused.
    let head = h
        .raw(&format!(
            "GET https://{dest}/x HTTP/1.1\r\nHost: {dest}\r\n\r\n"
        ))
        .await;
    assert!(head.starts_with("HTTP/1.1 400"), "{head}");
    let head = h
        .raw("GET http://other.example.net/x HTTP/1.1\r\nHost: other.example.net\r\n\r\n")
        .await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
}

#[tokio::test]
async fn routes_inject_the_secret_and_rewrite_the_path() {
    let up = Upstream::start().await;
    let h = Harness::start(true).await;
    let upstream = format!("http://127.0.0.1:{}/base", up.addr.port());
    h.register(
        NetworkMode::DenyAll,
        &[],
        vec![
            route("tok-anthropic", &upstream, "x-api-key", "sk-ant-real"),
            route("tok-openai", &upstream, "Authorization", "sk-oa-real"),
            route("tok-github", &upstream, "Authorization", "token ghp_real"),
        ],
    );
    let client = reqwest::Client::new();

    let resp = client
        .post(h.url("/r/tok-anthropic/v1/messages?beta=1"))
        .header("x-api-key", "warren-broker")
        .header("content-type", "application/json")
        .body("{\"model\":\"m\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["x-echo-path"], "/base/v1/messages?beta=1");
    assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");
    let seen = up.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/base/v1/messages?beta=1");
    assert_eq!(seen[0].header("x-api-key"), Some("sk-ant-real"), "placeholder replaced");
    assert_eq!(seen[0].header("content-type"), Some("application/json"));
    assert_eq!(seen[0].header("content-length"), Some("13"));
    assert_eq!(seen[0].body, "{\"model\":\"m\"}");
    assert_eq!(
        seen[0].header("host"),
        Some(format!("127.0.0.1:{}", up.addr.port()).as_str())
    );

    // Authorization gets a Bearer scheme unless the value has one.
    client.get(h.url("/r/tok-openai")).send().await.unwrap();
    client.get(h.url("/r/tok-github/user")).send().await.unwrap();
    let seen = up.seen();
    assert_eq!(seen[1].path, "/base");
    assert_eq!(seen[1].header("authorization"), Some("Bearer sk-oa-real"));
    assert_eq!(seen[2].path, "/base/user");
    assert_eq!(seen[2].header("authorization"), Some("token ghp_real"));

    // Unknown tokens, including one registered for another address, are 404.
    h.registry.register(BrokerEntry {
        instance_id: "zz99yy88xx".into(),
        ip: "10.9.9.9".parse().unwrap(),
        mode: NetworkMode::Allowlist,
        allowed_domains: vec![],
        routes: vec![route("tok-other", &upstream, "x-api-key", "other")],
    });
    for path in ["/r/tok-other/x", "/r/nope", "/r/", "/elsewhere"] {
        let resp = client.get(h.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    assert_eq!(up.seen().len(), 3);
}

#[tokio::test]
async fn registry_replaces_entries_by_id_and_address() {
    let registry = Registry::new();
    let entry = |id: &str, ip: &str| BrokerEntry {
        instance_id: id.into(),
        ip: ip.parse().unwrap(),
        mode: NetworkMode::Allowlist,
        allowed_domains: vec![],
        routes: vec![],
    };
    registry.register(entry("aaaaaaaaaa", "10.0.0.2"));
    registry.register(entry("bbbbbbbbbb", "10.0.0.3"));
    assert_eq!(registry.len(), 2);
    // Same instance moves to a new address.
    registry.register(entry("aaaaaaaaaa", "10.0.0.4"));
    assert!(registry.lookup("10.0.0.2".parse().unwrap()).is_none());
    assert_eq!(
        registry
            .lookup("10.0.0.4".parse().unwrap())
            .unwrap()
            .instance_id,
        "aaaaaaaaaa"
    );
    // Another instance takes an address: the stale owner goes.
    registry.register(entry("cccccccccc", "10.0.0.3"));
    assert!(registry.get("bbbbbbbbbb").is_none());
    assert_eq!(registry.len(), 2);
    registry.unregister("cccccccccc");
    registry.unregister("never-there");
    assert_eq!(registry.len(), 1);
    assert!(registry.lookup("10.0.0.3".parse().unwrap()).is_none());
    let dbg = format!(
        "{:?}",
        route("secret-token", "https://x.example.com", "x-api-key", "sk-value")
    );
    assert!(!dbg.contains("sk-value") && !dbg.contains("secret-token"));
}

#[tokio::test]
async fn oversized_route_bodies_are_refused() {
    let up = Upstream::start().await;
    let registry = Registry::new();
    let broker = Arc::new(Broker::new(
        registry.clone(),
        Options {
            allow_forbidden_destinations: true,
            max_route_body_bytes: 16,
            ..Options::default()
        },
    ));
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(serve(listener, broker, shutdown.clone()));
    registry.register(BrokerEntry {
        instance_id: "ab12cd34ef".into(),
        ip: LOCAL,
        mode: NetworkMode::Allowlist,
        allowed_domains: vec![],
        routes: vec![route(
            "tok",
            &format!("http://127.0.0.1:{}", up.addr.port()),
            "x-api-key",
            "v",
        )],
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/r/tok/x"))
        .body("x".repeat(17))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = client
        .post(format!("http://{addr}/r/tok/x"))
        .body("x".repeat(16))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(up.seen().len(), 1);
}
