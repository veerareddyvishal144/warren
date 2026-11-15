//! Unit tests: in-memory fakes for `Lifecycle` and `Authenticator`, the real
//! HMAC signer, a real upstream HTTP server on a loopback listener, and the
//! edge router driven in-process (tower `oneshot`) or over a loopback
//! listener when the scenario needs a live connection (upgrades, peer address).

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    routing::any,
};
use http::{
    HeaderValue, Method, StatusCode,
    header::{
        ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
        ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, CONNECTION,
        COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE, UPGRADE,
    },
};
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tower::ServiceExt;
use warren_core::{
    ApiKey, Authenticator, Backup, Clock, Config, CreateInstanceRequest, Error, ExecRequest,
    ExecResult, ForkRequest, HmacSigner, HostKind, Instance, InstanceStatus, Lifecycle,
    LogsOptions, MetricPoint, PatchInstanceRequest, RealClock, Result, ShapeRequest, Signer,
    Workspace,
};

use crate::{
    Deps, EdgeOptions, Route, auth::UNAUTHENTICATED, classify, headers, proxy::NO_DEFAULT_PORT,
    route, router_with,
};

const ID: &str = "ab12cd34ef";
const OTHER_ID: &str = "zz99yy88xx";
const KEY: &str = "sk_live_testkey";
const OTHER_KEY: &str = "sk_live_otherkey";
const WS: &str = "ws_one";
const OTHER_WS: &str = "ws_two";

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn unsupported() -> Error {
    Error::not_supported("not implemented by the fake")
}

// ---------------------------------------------------------------- fakes

struct FakeLifecycle {
    instances: Mutex<HashMap<String, Instance>>,
    ip: String,
    /// The per-instance gateway token `gateway_credentials` hands out.
    token: Mutex<String>,
    activity: AtomicUsize,
    awakened: AtomicUsize,
}

impl FakeLifecycle {
    fn new(ip: &str) -> Arc<Self> {
        Arc::new(FakeLifecycle {
            instances: Mutex::new(HashMap::new()),
            ip: ip.to_string(),
            token: Mutex::new(String::new()),
            activity: AtomicUsize::new(0),
            awakened: AtomicUsize::new(0),
        })
    }

    fn set_token(&self, token: &str) {
        *self.token.lock().unwrap() = token.to_string();
    }

    fn insert(&self, inst: Instance) {
        self.instances.lock().unwrap().insert(inst.id.clone(), inst);
    }

    fn lookup(&self, id: &str) -> Option<Instance> {
        self.instances.lock().unwrap().get(id).cloned()
    }
}

/// A `Lifecycle` method the edge never calls, with the lifetime structure the
/// `#[async_trait]` expansion gives the trait.
macro_rules! stub {
    ($name:ident < $($l:lifetime),+ > ( & $s:lifetime self $(, $arg:ident : $ty:ty)* ) -> $ret:ty) => {
        fn $name<$($l,)+ 'at>(& $s self $(, $arg: $ty)*) -> BoxFut<'at, Result<$ret>>
        where
            $($l: 'at,)+
            Self: 'at,
        {
            Box::pin(async { Err(unsupported()) })
        }
    };
}

impl Lifecycle for FakeLifecycle {
    stub!(create<'a, 'b, 'c>(&'a self, _ws: &'b Workspace, _req: &'c CreateInstanceRequest) -> Instance);

    fn get<'a, 'b, 'c, 'at>(
        &'a self,
        workspace_id: &'b str,
        id: &'c str,
    ) -> BoxFut<'at, Result<Instance>>
    where
        'a: 'at,
        'b: 'at,
        'c: 'at,
        Self: 'at,
    {
        let found = self.lookup(id).filter(|i| i.workspace_id == workspace_id);
        Box::pin(async move { found.ok_or_else(|| Error::not_found("instance")) })
    }

    stub!(list<'a, 'b>(&'a self, _workspace_id: &'b str) -> Vec<Instance>);
    stub!(patch<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _req: &'d PatchInstanceRequest) -> Instance);
    stub!(start<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> Instance);
    stub!(stop<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> Instance);
    stub!(sleep<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> Instance);
    stub!(restart<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> Instance);
    stub!(delete<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> ());
    stub!(resize<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _shape: &'d ShapeRequest) -> Instance);
    stub!(update<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _template: Option<&'d str>) -> Instance);
    stub!(fork<'a, 'b, 'c, 'd>(&'a self, _ws: &'b Workspace, _id: &'c str, _req: &'d ForkRequest) -> Instance);
    stub!(exec<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _req: &'d ExecRequest) -> ExecResult);
    stub!(logs<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str, _opts: LogsOptions) -> String);
    stub!(metrics<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str, _hours: u32) -> Vec<MetricPoint>);
    stub!(backup<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _note: &'d str) -> Backup);
    stub!(list_backups<'a, 'b, 'c>(&'a self, _workspace_id: &'b str, _id: &'c str) -> Vec<Backup>);
    stub!(restore<'a, 'b, 'c, 'd>(&'a self, _workspace_id: &'b str, _id: &'c str, _backup_id: &'d str) -> Instance);

    fn ensure_awake<'a, 'b, 'at>(&'a self, id: &'b str) -> BoxFut<'at, Result<Instance>>
    where
        'a: 'at,
        'b: 'at,
        Self: 'at,
    {
        self.awakened.fetch_add(1, Ordering::SeqCst);
        let found = self.lookup(id);
        Box::pin(async move {
            match found {
                None => Err(Error::not_found("instance")),
                Some(i) if matches!(i.status, InstanceStatus::Stopped | InstanceStatus::Failed) => {
                    Err(Error::conflict(format!("instance is {}", i.status)))
                }
                Some(mut i) => {
                    i.status = InstanceStatus::Running;
                    Ok(i)
                }
            }
        })
    }

    fn record_activity(&self, _id: &str) {
        self.activity.fetch_add(1, Ordering::SeqCst);
    }

    fn endpoint<'a, 'b, 'at>(&'a self, id: &'b str) -> BoxFut<'at, Result<String>>
    where
        'a: 'at,
        'b: 'at,
        Self: 'at,
    {
        let found = self.lookup(id).map(|_| self.ip.clone());
        Box::pin(async move { found.ok_or_else(|| Error::not_found("instance")) })
    }

    fn gateway_credentials<'a, 'b, 'at>(
        &'a self,
        id: &'b str,
    ) -> BoxFut<'at, Result<(String, String)>>
    where
        'a: 'at,
        'b: 'at,
        Self: 'at,
    {
        let token = self.token.lock().unwrap().clone();
        let found = self.lookup(id).map(|_| (self.ip.clone(), token));
        Box::pin(async move { found.ok_or_else(|| Error::not_found("instance")) })
    }
}

struct FakeAuth {
    keys: HashMap<String, Workspace>,
}

impl Authenticator for FakeAuth {
    fn authenticate<'a, 'b, 'at>(
        &'a self,
        raw_key: &'b str,
    ) -> BoxFut<'at, Result<(Workspace, ApiKey)>>
    where
        'a: 'at,
        'b: 'at,
        Self: 'at,
    {
        let mut key = raw_key.trim();
        if key.len() >= 7 && key[..7].eq_ignore_ascii_case("bearer ") {
            key = key[7..].trim();
        }
        let found = self.keys.get(key).cloned();
        Box::pin(async move {
            found
                .map(|ws| (ws, ApiKey::default()))
                .ok_or_else(|| Error::invalid_api_key("Invalid API key."))
        })
    }
}

// ------------------------------------------------------------- upstream

/// Echoes the request as JSON so tests can inspect what the instance saw.
async fn echo(req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in &parts.headers {
        headers
            .entry(name.as_str().to_string())
            .or_default()
            .push(value.to_str().unwrap_or("<binary>").to_string());
    }
    Json(json!({
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "query": parts.uri.query(),
        "headers": headers,
        "body_len": bytes.len(),
        "body": String::from_utf8_lossy(&bytes[..bytes.len().min(64)]),
    }))
    .into_response()
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(5)).await;
    "late"
}

async fn set_cookie() -> Response {
    ([(SET_COOKIE, "app=1; Path=/"), (CONNECTION, "close")], "ok").into_response()
}

/// A generic HTTP/1.1 upgrade: answers 101 and then echoes bytes upper-cased.
async fn upgrade(mut req: Request) -> Response {
    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        let mut io = TokioIo::new(upgraded);
        let mut buf = [0u8; 1024];
        while let Ok(n) = io.read(&mut buf).await {
            if n == 0 || io.write_all(&buf[..n].to_ascii_uppercase()).await.is_err() {
                break;
            }
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(UPGRADE, "echo")
        .header(CONNECTION, "Upgrade")
        .body(Body::empty())
        .unwrap()
}

async fn start_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/slow", any(slow))
        .route("/set-cookie", any(set_cookie))
        .route("/upgrade", any(upgrade))
        .fallback(echo);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

// -------------------------------------------------------------- harness

struct Harness {
    lifecycle: Arc<FakeLifecycle>,
    signer: Arc<HmacSigner>,
    cfg: Config,
    router: Router,
    upstream_port: u16,
    /// The most API requests `/v1/count` saw in flight at once.
    api_max_in_flight: Arc<AtomicUsize>,
}

/// An API route that sleeps `ms` milliseconds, then answers `late`.
async fn api_sleep(ms: u64) -> &'static str {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    "late"
}

fn instance(id: &str, ws: &str, port: u16) -> Instance {
    Instance {
        id: id.into(),
        workspace_id: ws.into(),
        status: InstanceStatus::Running,
        default_port: port,
        ..Default::default()
    }
}

async fn harness() -> Harness {
    harness_with(EdgeOptions::default()).await
}

async fn harness_with(opts: EdgeOptions) -> Harness {
    let upstream_port = start_upstream().await;
    let lifecycle = FakeLifecycle::new("127.0.0.1");
    lifecycle.insert(instance(ID, WS, upstream_port));
    lifecycle.insert(instance(OTHER_ID, OTHER_WS, upstream_port));
    let keys = HashMap::from([
        (
            KEY.to_string(),
            Workspace {
                id: WS.into(),
                ..Default::default()
            },
        ),
        (
            OTHER_KEY.to_string(),
            Workspace {
                id: OTHER_WS.into(),
                ..Default::default()
            },
        ),
    ]);
    let signer = Arc::new(HmacSigner::new("test-secret", Arc::new(RealClock)));
    let cfg = Config::default();
    let api_max_in_flight = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let counter = {
        let (in_flight, max) = (in_flight.clone(), api_max_in_flight.clone());
        move || {
            let (in_flight, max) = (in_flight.clone(), max.clone());
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(60)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                "counted"
            }
        }
    };
    let api = Router::new()
        .route(
            "/v1/health",
            any(|| async { Json(json!({ "healthy": true })) }),
        )
        .route("/v1/slow", any(|| api_sleep(400)))
        .route("/v1/instances/{id}/exec", any(|| api_sleep(400)))
        .route("/v1/count", any(counter))
        .fallback(|| async { (StatusCode::NOT_FOUND, "api 404") });
    let dashboard = Router::new().fallback(|| async { "dashboard" });
    let router = router_with(
        Deps {
            config: cfg.clone(),
            api,
            dashboard,
            lifecycle: lifecycle.clone(),
            auth: Arc::new(FakeAuth { keys }),
            signer: signer.clone(),
        },
        opts,
    );
    Harness {
        lifecycle,
        signer,
        cfg,
        router,
        upstream_port,
        api_max_in_flight,
    }
}

impl Harness {
    fn host(&self) -> String {
        format!("{ID}.warren.localhost:7337")
    }

    /// The instance's own origin on the default-port host.
    fn origin(&self) -> String {
        format!("http://{ID}.warren.localhost:7337")
    }

    /// A cookie-authenticated GET with extra headers.
    fn cookie_get(&self, extra: &[(&str, &str)]) -> Request {
        let mut b = self
            .get(&self.host(), "/")
            .header(COOKIE, format!("warren_session={}", self.token(ID, 0)));
        for (name, value) in extra {
            b = b.header(*name, *value);
        }
        b.body(Body::empty()).unwrap()
    }

    fn port_host(&self, port: u16) -> String {
        format!("{ID}-{port}.warren.localhost:7337")
    }

    fn get(&self, host: &str, target: &str) -> http::request::Builder {
        Request::builder()
            .method(Method::GET)
            .uri(target)
            .header(HOST, host)
    }

    async fn send(&self, req: Request) -> Response {
        self.router.clone().oneshot(req).await.unwrap()
    }

    fn token(&self, id: &str, port: u16) -> String {
        self.signer.sign(id, port, RealClock.now_unix() + 3600)
    }

    /// Serve the edge on a loopback listener for scenarios that need a socket.
    async fn listen(&self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.router.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        addr
    }
}

async fn json_body(resp: Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)))
}

async fn text_body(resp: Response) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

fn seen_header<'a>(echo: &'a Value, name: &str) -> Option<&'a str> {
    echo["headers"][name][0].as_str()
}

fn header_str<'a>(resp: &'a Response, name: &http::HeaderName) -> &'a str {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or("")
}

async fn read_head(tcp: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        tcp.read_exact(&mut byte).await.unwrap();
        buf.push(byte[0]);
    }
    String::from_utf8(buf).unwrap()
}

// ------------------------------------------------------------ pure parts

#[test]
fn classify_and_route() {
    let cfg = Config::default();
    assert_eq!(classify(&cfg, "api.warren.localhost:7337"), HostKind::Api);
    assert_eq!(
        classify(&cfg, "ab12cd34ef-7681.warren.localhost"),
        HostKind::Instance {
            id: ID.into(),
            port: 7681
        }
    );
    assert_eq!(
        route(&cfg, "warren.localhost:7337", "/v1/health"),
        Route::Api
    );
    assert_eq!(route(&cfg, "warren.localhost:7337", "/v1"), Route::Api);
    assert_eq!(
        route(&cfg, "warren.localhost:7337", "/v1x"),
        Route::Dashboard
    );
    assert_eq!(route(&cfg, "warren.localhost:7337", "/"), Route::Dashboard);
    assert_eq!(route(&cfg, "api.warren.localhost", "/anything"), Route::Api);
    assert_eq!(
        route(&cfg, "ab12cd34ef.warren.localhost", "/v1/health"),
        Route::Instance {
            id: ID.into(),
            port: 0
        }
    );
    assert_eq!(route(&cfg, "", "/"), Route::Dashboard);
}

#[test]
fn header_helpers() {
    assert_eq!(
        headers::split_token(Some("a=1&warren_token=tok.sig&b=2&warren_token=dup")),
        Some(("tok.sig".to_string(), "a=1&b=2".to_string()))
    );
    assert_eq!(
        headers::split_token(Some("warren_token=only")),
        Some(("only".to_string(), String::new()))
    );
    assert_eq!(headers::split_token(Some("a=1")), None);
    assert_eq!(headers::split_token(None), None);

    let mut h = http::HeaderMap::new();
    h.append(
        COOKIE,
        HeaderValue::from_static("a=1; warren_session=tok; b=x=y"),
    );
    h.append(COOKIE, HeaderValue::from_static("c=3"));
    assert_eq!(headers::session_cookie(&h).as_deref(), Some("tok"));
    assert_eq!(
        headers::cookies_without_session(&h)
            .unwrap()
            .to_str()
            .unwrap(),
        "a=1; b=x=y; c=3"
    );
    let mut only = http::HeaderMap::new();
    only.insert(COOKIE, HeaderValue::from_static("warren_session=tok"));
    assert!(headers::cookies_without_session(&only).is_none());

    let mut xff = http::HeaderMap::new();
    xff.insert(
        headers::X_FORWARDED_FOR,
        HeaderValue::from_static("10.0.0.1, 10.0.0.2"),
    );
    let peer = Some("192.168.1.9".parse().unwrap());
    assert_eq!(
        headers::forwarded_for(&xff, peer)
            .unwrap()
            .to_str()
            .unwrap(),
        "10.0.0.1, 10.0.0.2, 192.168.1.9"
    );
    assert!(headers::forwarded_for(&http::HeaderMap::new(), None).is_none());

    assert_eq!(headers::authority("10.1.2.3", 80), "10.1.2.3:80");
    assert_eq!(headers::authority("fd00::1", 3737), "[fd00::1]:3737");
    assert!(headers::is_hop_by_hop(&http::header::TRANSFER_ENCODING));
    assert!(!headers::is_hop_by_hop(&http::header::AUTHORIZATION));
}

// -------------------------------------------------------------- dispatch

#[tokio::test]
async fn dispatches_api_and_dashboard_by_host_and_path() {
    let h = harness().await;
    let resp = h
        .send(
            h.get("api.warren.localhost:7337", "/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_body(resp).await["healthy"], true);

    let resp = h
        .send(
            h.get("warren.localhost:7337", "/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = h
        .send(
            h.get("warren.localhost:7337", "/")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(text_body(resp).await, "dashboard");

    let resp = h
        .send(
            h.get("api.warren.localhost", "/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(text_body(resp).await, "api 404");
}

// ----------------------------------------------------------------- proxy

#[tokio::test]
async fn proxies_with_api_key_and_rewrites_headers() {
    let h = harness().await;
    let req = h
        .get(&h.host(), "/hello/world?x=1&y=2")
        .header("x-warren-key", KEY)
        .header(AUTHORIZATION, "Basic abc")
        .header(COOKIE, "a=1; warren_session=zzz; b=2")
        .header(CONNECTION, "keep-alive, x-private")
        .header("x-private", "secret")
        .header("x-forwarded-for", "10.0.0.1")
        .header("x-forwarded-proto", "https")
        .header("x-custom", "kept")
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(seen["method"], "GET");
    assert_eq!(seen["path"], "/hello/world");
    assert_eq!(seen["query"], "x=1&y=2");
    assert_eq!(
        seen_header(&seen, "host"),
        Some(format!("127.0.0.1:{}", h.upstream_port).as_str())
    );
    assert_eq!(
        seen_header(&seen, "x-forwarded-host"),
        Some(h.host().as_str())
    );
    assert_eq!(seen_header(&seen, "x-forwarded-proto"), Some("http"));
    assert_eq!(seen_header(&seen, "x-forwarded-for"), Some("10.0.0.1"));
    assert_eq!(seen_header(&seen, "authorization"), Some("Basic abc"));
    assert_eq!(seen_header(&seen, "cookie"), Some("a=1; b=2"));
    assert_eq!(seen_header(&seen, "x-custom"), Some("kept"));
    assert_eq!(
        seen_header(&seen, "x-warren-key"),
        None,
        "the key never reaches the instance"
    );
    assert_eq!(
        seen_header(&seen, "x-private"),
        None,
        "headers nominated by Connection are hop-by-hop"
    );
    assert_eq!(seen_header(&seen, "connection"), None);
    assert!(
        h.lifecycle.activity.load(Ordering::SeqCst) >= 2,
        "activity is recorded on request and response"
    );
    assert_eq!(h.lifecycle.awakened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proxies_request_bodies_and_explicit_ports() {
    let h = harness().await;
    let payload = "x".repeat(300 * 1024);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/upload")
        .header(HOST, h.port_host(h.upstream_port))
        .header("x-warren-key", KEY)
        .header("content-type", "text/plain")
        .body(Body::from(payload.clone()))
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(seen["method"], "POST");
    assert_eq!(seen["body_len"], payload.len());
    assert_eq!(seen_header(&seen, "content-type"), Some("text/plain"));
    assert_eq!(
        seen_header(&seen, "host"),
        Some(format!("127.0.0.1:{}", h.upstream_port).as_str())
    );
}

#[tokio::test]
async fn relays_upstream_headers_and_drops_hop_by_hop() {
    let h = harness().await;
    let resp = h
        .send(
            h.get(&h.host(), "/set-cookie")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, &SET_COOKIE), "app=1; Path=/");
    assert!(
        resp.headers().get(CONNECTION).is_none(),
        "Connection is hop-by-hop"
    );
    assert_eq!(text_body(resp).await, "ok");
}

#[tokio::test]
async fn authorization_bearer_key_authenticates_and_is_stripped() {
    let h = harness().await;
    let req = h
        .get(&h.host(), "/")
        .header(AUTHORIZATION, format!("Bearer {KEY}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(
        seen_header(&seen, "authorization"),
        None,
        "a Warren key never reaches the instance"
    );

    // Also when X-Warren-Key did the authenticating: the bearer is still a key.
    let req = h
        .get(&h.host(), "/")
        .header("x-warren-key", KEY)
        .header(AUTHORIZATION, "Bearer sk_live_somethingelse")
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(seen_header(&seen, "authorization"), None);
    assert_eq!(seen_header(&seen, "x-warren-key"), None);

    // A non-Warren bearer under key auth is the app's own and is forwarded.
    let req = h
        .get(&h.host(), "/")
        .header("x-warren-key", KEY)
        .header(AUTHORIZATION, "Bearer app-token")
        .body(Body::empty())
        .unwrap();
    let seen = json_body(h.send(req).await).await;
    assert_eq!(
        seen_header(&seen, "authorization"),
        Some("Bearer app-token")
    );
}

#[tokio::test]
async fn instance_token_is_injected_and_never_client_supplied() {
    let h = harness().await;
    // No token known: a client-sent header is dropped.
    let req = h
        .get(&h.host(), "/")
        .header("x-warren-key", KEY)
        .header("x-warren-instance-token", "forged")
        .body(Body::empty())
        .unwrap();
    let seen = json_body(h.send(req).await).await;
    assert_eq!(seen_header(&seen, "x-warren-instance-token"), None);

    // A known token is set, overwriting whatever the client sent.
    h.lifecycle.set_token("it_secret");
    let req = h
        .get(&h.host(), "/")
        .header("x-warren-key", KEY)
        .header("x-warren-instance-token", "forged")
        .body(Body::empty())
        .unwrap();
    let seen = json_body(h.send(req).await).await;
    assert_eq!(
        seen["headers"]["x-warren-instance-token"],
        json!(["it_secret"])
    );
}

#[tokio::test]
async fn session_cookie_requires_same_origin() {
    let h = harness().await;
    let own = h.origin();
    let ok = [
        vec![],
        vec![("origin", own.as_str())],
        vec![("referer", "http://ab12cd34ef.warren.localhost:7337/app?x=1")],
        vec![("sec-fetch-site", "same-origin")],
        vec![("sec-fetch-site", "none")],
        // A top-level navigation (what SameSite=Lax permits) from the dashboard.
        vec![
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
            ("referer", "http://warren.localhost:7337/"),
        ],
    ];
    for extra in &ok {
        let resp = h.send(h.cookie_get(extra)).await;
        assert_eq!(resp.status(), StatusCode::OK, "{extra:?}");
    }
    let refused = [
        vec![("origin", "http://warren.localhost:7337")],
        vec![("origin", "http://zz99yy88xx.warren.localhost:7337")],
        vec![("origin", "http://evil.example")],
        vec![("referer", "http://zz99yy88xx.warren.localhost:7337/")],
        vec![("sec-fetch-site", "same-site")],
        vec![("sec-fetch-site", "cross-site")],
        // Embedding (an iframe) is not a top-level navigation.
        vec![
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "iframe"),
        ],
    ];
    for extra in &refused {
        let resp = h.send(h.cookie_get(extra)).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{extra:?}");
        let body = json_body(resp).await;
        assert_eq!(body["error"]["code"], "invalid_api_key", "{extra:?}");
    }
    // A POST is never a navigation, even with navigation headers.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/exec")
        .header(HOST, h.host())
        .header(COOKIE, format!("warren_session={}", h.token(ID, 0)))
        .header("sec-fetch-site", "same-site")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.status(), StatusCode::UNAUTHORIZED);
    // Keys are not subject to the check: scripts on other origins hold one.
    let req = h
        .get(&h.host(), "/")
        .header("x-warren-key", KEY)
        .header("origin", "http://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.status(), StatusCode::OK);
    assert_eq!(
        h.lifecycle.awakened.load(Ordering::SeqCst),
        ok.len() + 1,
        "refused requests never touch the instance"
    );
}

#[tokio::test]
async fn foreign_bearer_passes_through_under_a_session() {
    let h = harness().await;
    let req = h
        .get(&h.host(), "/")
        .header(AUTHORIZATION, "Bearer app-token")
        .header(COOKIE, format!("warren_session={}", h.token(ID, 0)))
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(
        seen_header(&seen, "authorization"),
        Some("Bearer app-token")
    );
    assert_eq!(seen_header(&seen, "cookie"), None);

    // Without a session a foreign bearer is an invalid key.
    let req = h
        .get(&h.host(), "/")
        .header(AUTHORIZATION, "Bearer app-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.status(), StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ auth

#[tokio::test]
async fn unauthenticated_requests_get_401_json() {
    let h = harness().await;
    let resp = h
        .send(h.get(&h.host(), "/").body(Body::empty()).unwrap())
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = json_body(resp).await;
    assert_eq!(body["error"]["code"], "invalid_api_key");
    assert_eq!(body["error"]["message"], UNAUTHENTICATED);
    assert_eq!(
        h.lifecycle.awakened.load(Ordering::SeqCst),
        0,
        "nothing is woken without auth"
    );

    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", "sk_live_wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(resp).await["error"]["code"], "invalid_api_key");

    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header(COOKIE, "warren_session=garbage")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(resp).await["error"]["code"], "invalid_api_key");

    let expired = h.signer.sign(ID, 0, RealClock.now_unix() - 10);
    let resp = h
        .send(
            h.get(&h.host(), &format!("/?warren_token={expired}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signed_url_redirects_with_cookie_then_cookie_proxies() {
    let h = harness().await;
    let token = h.token(ID, 0);
    let resp = h
        .send(
            h.get(&h.host(), &format!("/term?a=1&warren_token={token}&b=2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        header_str(&resp, &LOCATION),
        "http://ab12cd34ef.warren.localhost:7337/term?a=1&b=2"
    );
    assert_eq!(
        header_str(&resp, &SET_COOKIE),
        format!("warren_session={token}; HttpOnly; SameSite=Lax; Path=/")
    );
    assert_eq!(
        h.lifecycle.awakened.load(Ordering::SeqCst),
        0,
        "the redirect does not wake the instance"
    );

    let resp = h
        .send(
            h.get(&h.host(), &format!("/term?warren_token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        header_str(&resp, &LOCATION),
        "http://ab12cd34ef.warren.localhost:7337/term"
    );

    // A path that could become a protocol-relative URL is refused.
    let resp = h
        .send(
            h.get(&h.host(), &format!("//evil.example/?warren_token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(resp.headers().get(LOCATION).is_none());
    assert_eq!(json_body(resp).await["error"]["code"], "invalid_request");

    let req = h
        .get(&h.host(), "/term?a=1&b=2")
        .header(COOKIE, format!("other=1; warren_session={token}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let seen = json_body(resp).await;
    assert_eq!(seen["path"], "/term");
    assert_eq!(seen["query"], "a=1&b=2");
    assert_eq!(seen_header(&seen, "cookie"), Some("other=1"));
}

#[tokio::test]
async fn signed_url_must_match_instance_and_port() {
    let h = harness().await;
    let resp = h
        .send(
            h.get(
                &h.host(),
                &format!("/?warren_token={}", h.token(OTHER_ID, 0)),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(resp).await["error"]["code"], "invalid_api_key");

    let resp = h
        .send(
            h.get(&h.host(), &format!("/?warren_token={}", h.token(ID, 7681)))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a port token is not valid on the default-port host"
    );

    let resp = h
        .send(
            h.get(
                &h.port_host(7681),
                &format!("/?warren_token={}", h.token(ID, 7681)),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FOUND);

    let req = h
        .get(&h.port_host(7681), "/")
        .header(COOKIE, format!("warren_session={}", h.token(ID, 0)))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        h.send(req).await.status(),
        StatusCode::UNAUTHORIZED,
        "a default-port cookie is not valid on a port host"
    );
}

#[tokio::test]
async fn https_sessions_set_secure_cookies() {
    let cfg = Config {
        public_scheme: "https".into(),
        ..Config::default()
    };
    assert_eq!(
        crate::auth::session_cookie(&cfg, "t"),
        "warren_session=t; HttpOnly; SameSite=Lax; Path=/; Secure"
    );
    assert_eq!(
        crate::auth::session_cookie(&Config::default(), "t"),
        "warren_session=t; HttpOnly; SameSite=Lax; Path=/"
    );
}

#[tokio::test]
async fn instance_of_another_workspace_is_404() {
    let h = harness().await;
    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", OTHER_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(resp).await["error"]["code"], "not_found");
    assert_eq!(h.lifecycle.awakened.load(Ordering::SeqCst), 0);
}

// ------------------------------------------------------- lifecycle errors

#[tokio::test]
async fn no_default_port_is_502_text() {
    let h = harness().await;
    h.lifecycle.insert(instance(ID, WS, 0));
    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(text_body(resp).await, NO_DEFAULT_PORT);
}

#[tokio::test]
async fn stopped_instance_is_409_and_unknown_is_404() {
    let h = harness().await;
    let mut stopped = instance(ID, WS, h.upstream_port);
    stopped.status = InstanceStatus::Stopped;
    h.lifecycle.insert(stopped);
    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(resp).await["error"]["code"], "conflict");

    let host = "qq11ww22ee.warren.localhost:7337";
    let token = h.token("qq11ww22ee", 0);
    let resp = h
        .send(
            h.get(host, "/")
                .header(COOKIE, format!("warren_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nothing_listening_is_502_bad_gateway() {
    let h = harness().await;
    h.lifecycle.insert(instance(ID, WS, free_port().await));
    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(json_body(resp).await["error"]["code"], "bad_gateway");
}

#[tokio::test]
async fn slow_upstream_is_504_timeout() {
    let h = harness_with(EdgeOptions {
        response_timeout: Duration::from_millis(300),
        ..EdgeOptions::default()
    })
    .await;
    let resp = h
        .send(
            h.get(&h.host(), "/slow")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(json_body(resp).await["error"]["code"], "timeout");
}

// ------------------------------------------------------------------ cors

#[tokio::test]
async fn cors_preflight_and_response_headers() {
    let h = harness().await;
    let dashboard = "http://warren.localhost:7337";
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/v1/responses")
        .header(HOST, h.host())
        .header(ORIGIN, dashboard)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "x-warren-key, x-app-trace",
        )
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_str(&resp, &ACCESS_CONTROL_ALLOW_ORIGIN), dashboard);
    assert_eq!(
        header_str(&resp, &ACCESS_CONTROL_ALLOW_HEADERS),
        "X-Warren-Key, Content-Type, Authorization, x-app-trace"
    );
    assert_eq!(
        header_str(&resp, &ACCESS_CONTROL_ALLOW_METHODS),
        "GET, POST, PUT, PATCH, DELETE, OPTIONS"
    );
    assert_eq!(header_str(&resp, &ACCESS_CONTROL_ALLOW_CREDENTIALS), "true");
    assert_eq!(
        h.lifecycle.awakened.load(Ordering::SeqCst),
        0,
        "preflights never touch the instance"
    );

    // A preflight from an untrusted origin is answered without any grant.
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/v1/responses")
        .header(HOST, h.host())
        .header(ORIGIN, "http://app.example")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(resp.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    assert!(resp.headers().get(ACCESS_CONTROL_ALLOW_HEADERS).is_none());
    assert!(
        resp.headers()
            .get(ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none()
    );

    // Trusted origins on proxied responses: the dashboard and the instance itself.
    for origin in [dashboard, &h.origin()] {
        let req = h
            .get(&h.host(), "/")
            .header("x-warren-key", KEY)
            .header(ORIGIN, origin)
            .body(Body::empty())
            .unwrap();
        let resp = h.send(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header_str(&resp, &ACCESS_CONTROL_ALLOW_ORIGIN), origin);
        assert_eq!(header_str(&resp, &ACCESS_CONTROL_ALLOW_CREDENTIALS), "true");
    }
    // On a port host the preview origin is trusted as well.
    let port_origin = format!("http://{ID}-{}.warren.localhost:7337", h.upstream_port);
    let req = h
        .get(&h.port_host(h.upstream_port), "/")
        .header("x-warren-key", KEY)
        .header(ORIGIN, &port_origin)
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header_str(&resp, &ACCESS_CONTROL_ALLOW_ORIGIN), port_origin);

    // Other origins, another instance included, get no CORS at all.
    for origin in [
        "http://app.example",
        "http://zz99yy88xx.warren.localhost:7337",
        "http://api.warren.localhost:7337",
        "https://warren.localhost:7337",
    ] {
        let req = h
            .get(&h.host(), "/")
            .header("x-warren-key", KEY)
            .header(ORIGIN, origin)
            .body(Body::empty())
            .unwrap();
        let resp = h.send(req).await;
        assert_eq!(resp.status(), StatusCode::OK, "{origin}");
        assert!(
            resp.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
            "{origin} must not be granted"
        );
        assert!(
            resp.headers()
                .get(ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none()
        );
    }

    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header(ORIGIN, dashboard)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_str(&resp, &ACCESS_CONTROL_ALLOW_ORIGIN),
        dashboard,
        "the dashboard can read edge errors too"
    );

    let resp = h
        .send(
            h.get(&h.host(), "/")
                .header("x-warren-key", KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(
        resp.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "no Origin, no CORS headers"
    );
}

#[test]
fn root_origin_follows_the_public_port_rule() {
    let cfg = Config::default();
    assert_eq!(
        crate::cors::root_origin(&cfg),
        "http://warren.localhost:7337"
    );
    let cfg = Config {
        public_scheme: "https".into(),
        public_port: 443,
        base_domain: "warren.example".into(),
        ..Config::default()
    };
    assert_eq!(crate::cors::root_origin(&cfg), "https://warren.example");
    let allowed = crate::cors::allowed_origins(&cfg, ID, 8080);
    assert_eq!(
        allowed,
        vec![
            "https://warren.example".to_string(),
            format!("https://{ID}.warren.example"),
            format!("https://{ID}-8080.warren.example"),
        ]
    );
    assert_eq!(crate::cors::allowed_origins(&cfg, ID, 0).len(), 2);
}

#[test]
fn exec_paths_are_recognised() {
    use crate::dispatch::is_exec_path;
    assert!(is_exec_path("/v1/instances/ab12cd34ef/exec"));
    assert!(is_exec_path("/v1/instances/ab12cd34ef/exec/"));
    assert!(!is_exec_path("/v1/instances/ab12cd34ef/logs"));
    assert!(!is_exec_path("/v1/instances//exec"));
    assert!(!is_exec_path("/v1/instances/ab12cd34ef/exec/more"));
    assert!(!is_exec_path("/v1/exec"));
}

#[tokio::test]
async fn api_requests_have_a_deadline_except_exec() {
    let h = harness_with(EdgeOptions {
        api_timeout: Duration::from_millis(100),
        ..EdgeOptions::default()
    })
    .await;
    let resp = h
        .send(
            h.get("api.warren.localhost:7337", "/v1/slow")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = json_body(resp).await;
    assert_eq!(body["error"]["code"], "timeout");

    // The same deadline applies under <base>/v1.
    let resp = h
        .send(
            h.get("warren.localhost:7337", "/v1/slow")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

    // exec runs to its own timeout.
    let resp = h
        .send(
            h.get("api.warren.localhost:7337", "/v1/instances/ab12cd34ef/exec")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(text_body(resp).await, "late");

    // Proxied instance requests are not subject to it either.
    let req = h
        .get(&h.host(), "/slow")
        .header("x-warren-key", KEY)
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn in_flight_requests_are_capped() {
    let h = harness_with(EdgeOptions {
        max_in_flight: 2,
        ..EdgeOptions::default()
    })
    .await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let router = h.router.clone();
        tasks.spawn(async move {
            let req = Request::builder()
                .method(Method::GET)
                .uri("/v1/count")
                .header(HOST, "api.warren.localhost:7337")
                .body(Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        });
    }
    while let Some(status) = tasks.join_next().await {
        assert_eq!(status.unwrap(), StatusCode::OK);
    }
    let max = h.api_max_in_flight.load(Ordering::SeqCst);
    assert!((1..=2).contains(&max), "saw {max} in flight");
}

// ------------------------------------------------------- live connections

#[tokio::test]
async fn upgrade_is_tunnelled_both_ways() {
    let h = harness().await;
    let addr = h.listen().await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(
        format!("GET /upgrade HTTP/1.1\r\nHost: {}\r\nX-Warren-Key: {KEY}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n", h.host())
            .as_bytes(),
    )
    .await
    .unwrap();
    let head = read_head(&mut tcp).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("upgrade: echo"),
        "{head}"
    );

    tcp.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"HELLO");
    tcp.write_all(b"again").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"AGAIN");
}

#[tokio::test]
async fn upgrade_requires_auth() {
    let h = harness().await;
    let addr = h.listen().await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(
        format!(
            "GET /upgrade HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n",
            h.host()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let head = read_head(&mut tcp).await;
    assert!(head.starts_with("HTTP/1.1 401"), "{head}");
}

#[tokio::test]
async fn served_requests_carry_the_peer_in_x_forwarded_for() {
    let h = harness().await;
    let addr = h.listen().await;
    let client: Client<_, Body> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .uri(format!("http://{addr}/peer"))
        .header(HOST, h.host())
        .header("x-warren-key", KEY)
        .body(Body::empty())
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let seen: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(seen_header(&seen, "x-forwarded-for"), Some("127.0.0.1"));
    assert_eq!(
        seen_header(&seen, "x-forwarded-host"),
        Some(h.host().as_str())
    );
    assert_eq!(h.cfg.base_domain, "warren.localhost");
}
