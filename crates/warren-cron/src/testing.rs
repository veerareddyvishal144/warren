//! In-memory fakes for the traits the scheduler consumes and a minimal HTTP/1.1
//! server on a raw `TcpListener` standing in for the gateway inside an
//! instance (axum and hyper are not dependencies of this crate).

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use warren_core::*;

/// A clock that returns whatever it was set to.
pub struct FixedClock(AtomicI64);

impl FixedClock {
    pub fn at(now: i64) -> Arc<Self> {
        Arc::new(FixedClock(AtomicI64::new(now)))
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn unsupported<T>(what: &str) -> Result<T> {
    Err(Error::internal(format!(
        "the fake does not implement {what}"
    )))
}

/// Holds crons in memory; everything else is unsupported.
#[derive(Default)]
pub struct FakeStore {
    pub crons: Mutex<Vec<Cron>>,
    /// Every cron passed to `update_cron`, in order.
    pub updates: Mutex<Vec<Cron>>,
    /// `list_due_crons` fails this many more times before working again.
    pub list_failures: AtomicUsize,
    /// Return disabled crons from `list_due_crons` too (a misbehaving store).
    pub include_disabled: AtomicBool,
    pub list_calls: AtomicUsize,
    /// `update_cron` fails this many more times before working again.
    pub update_failures: AtomicUsize,
    pub update_calls: AtomicUsize,
}

impl FakeStore {
    pub fn with_crons(crons: Vec<Cron>) -> Arc<Self> {
        Arc::new(FakeStore {
            crons: Mutex::new(crons),
            ..Default::default()
        })
    }
    /// Current state of one cron.
    pub fn cron(&self, id: &str) -> Cron {
        self.crons
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .unwrap_or_else(|| panic!("no cron {id}"))
    }
    pub fn update_count(&self) -> usize {
        self.updates.lock().unwrap().len()
    }
}

#[async_trait]
impl Store for FakeStore {
    async fn create_workspace(&self, _ws: &Workspace) -> Result<()> {
        unsupported("create_workspace")
    }
    async fn get_workspace(&self, _id: &str) -> Result<Workspace> {
        unsupported("get_workspace")
    }
    async fn update_workspace(&self, _ws: &Workspace) -> Result<()> {
        unsupported("update_workspace")
    }
    async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        unsupported("list_workspaces")
    }
    async fn create_api_key(&self, _k: &ApiKey) -> Result<()> {
        unsupported("create_api_key")
    }
    async fn get_api_key_by_hash(&self, _hash: &str) -> Result<ApiKey> {
        unsupported("get_api_key_by_hash")
    }
    async fn list_api_keys(&self, _workspace_id: &str) -> Result<Vec<ApiKey>> {
        unsupported("list_api_keys")
    }
    async fn delete_api_key(&self, _workspace_id: &str, _id: &str) -> Result<()> {
        unsupported("delete_api_key")
    }
    async fn touch_api_key(&self, _id: &str, _at: i64) -> Result<()> {
        unsupported("touch_api_key")
    }
    async fn upsert_template(&self, _t: &Template) -> Result<()> {
        unsupported("upsert_template")
    }
    async fn get_template(&self, _workspace_id: &str, _name: &str) -> Result<Template> {
        unsupported("get_template")
    }
    async fn get_template_revision(
        &self,
        _workspace_id: &str,
        _name: &str,
        _revision: u32,
    ) -> Result<Template> {
        unsupported("get_template_revision")
    }
    async fn list_templates(&self, _workspace_id: &str) -> Result<Vec<Template>> {
        unsupported("list_templates")
    }
    async fn delete_template(&self, _workspace_id: &str, _name: &str) -> Result<()> {
        unsupported("delete_template")
    }
    async fn create_instance(&self, _inst: &Instance) -> Result<()> {
        unsupported("create_instance")
    }
    async fn get_instance(&self, _id: &str) -> Result<Instance> {
        unsupported("get_instance")
    }
    async fn get_instance_for_workspace(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("get_instance_for_workspace")
    }
    async fn list_instances(&self, _workspace_id: &str) -> Result<Vec<Instance>> {
        unsupported("list_instances")
    }
    async fn list_all_instances(&self) -> Result<Vec<Instance>> {
        unsupported("list_all_instances")
    }
    async fn update_instance(&self, _inst: &Instance) -> Result<()> {
        unsupported("update_instance")
    }
    async fn delete_instance(&self, _id: &str) -> Result<()> {
        unsupported("delete_instance")
    }
    async fn count_awake_instances(&self, _workspace_id: &str) -> Result<u32> {
        unsupported("count_awake_instances")
    }
    async fn count_instances_using_template(
        &self,
        _workspace_id: &str,
        _name: &str,
    ) -> Result<u32> {
        unsupported("count_instances_using_template")
    }
    async fn replace_instance_secrets(
        &self,
        _instance_id: &str,
        _secrets: &[InstanceSecret],
    ) -> Result<()> {
        unsupported("replace_instance_secrets")
    }
    async fn list_instance_secrets(&self, _instance_id: &str) -> Result<Vec<InstanceSecret>> {
        unsupported("list_instance_secrets")
    }
    async fn insert_metric(&self, _instance_id: &str, _p: MetricPoint) -> Result<()> {
        unsupported("insert_metric")
    }
    async fn list_metrics(&self, _instance_id: &str, _since: i64) -> Result<Vec<MetricPoint>> {
        unsupported("list_metrics")
    }
    async fn prune_metrics(&self, _before: i64) -> Result<()> {
        unsupported("prune_metrics")
    }
    async fn create_backup(&self, _b: &Backup) -> Result<()> {
        unsupported("create_backup")
    }
    async fn list_backups(&self, _instance_id: &str) -> Result<Vec<Backup>> {
        unsupported("list_backups")
    }
    async fn get_backup(&self, _instance_id: &str, _id: &str) -> Result<Backup> {
        unsupported("get_backup")
    }
    async fn delete_backups_for_instance(&self, _instance_id: &str) -> Result<()> {
        unsupported("delete_backups_for_instance")
    }

    async fn create_cron(&self, c: &Cron) -> Result<()> {
        self.crons.lock().unwrap().push(c.clone());
        Ok(())
    }
    async fn get_cron(&self, instance_id: &str, id: &str) -> Result<Cron> {
        self.crons
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.instance_id == instance_id && c.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found("cron"))
    }
    async fn list_crons(&self, instance_id: &str) -> Result<Vec<Cron>> {
        Ok(self
            .crons
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.instance_id == instance_id)
            .cloned()
            .collect())
    }
    async fn list_due_crons(&self, now: i64) -> Result<Vec<Cron>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.list_failures.load(Ordering::SeqCst) > 0 {
            self.list_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::internal("database is locked"));
        }
        let include_disabled = self.include_disabled.load(Ordering::SeqCst);
        Ok(self
            .crons
            .lock()
            .unwrap()
            .iter()
            .filter(|c| (c.enabled || include_disabled) && c.next_run_at <= now)
            .cloned()
            .collect())
    }
    async fn update_cron(&self, c: &Cron) -> Result<()> {
        self.update_calls.fetch_add(1, Ordering::SeqCst);
        if self.update_failures.load(Ordering::SeqCst) > 0 {
            self.update_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::internal("database is locked"));
        }
        let mut crons = self.crons.lock().unwrap();
        let slot = crons
            .iter_mut()
            .find(|x| x.id == c.id)
            .ok_or_else(|| Error::not_found("cron"))?;
        *slot = c.clone();
        self.updates.lock().unwrap().push(c.clone());
        Ok(())
    }
    async fn delete_cron(&self, instance_id: &str, id: &str) -> Result<()> {
        self.crons
            .lock()
            .unwrap()
            .retain(|c| !(c.instance_id == instance_id && c.id == id));
        Ok(())
    }
}

/// Records wake calls and hands out a fixed endpoint; everything else is unsupported.
pub struct FakeLifecycle {
    pub endpoint: Mutex<String>,
    /// The gateway token `gateway_credentials` returns; empty by default.
    pub token: Mutex<String>,
    pub wake_error: Mutex<Option<Error>>,
    pub endpoint_error: Mutex<Option<Error>>,
    pub awake_calls: Mutex<Vec<String>>,
    pub activity: AtomicUsize,
}

impl FakeLifecycle {
    /// Every instance resolves to `endpoint` (an IP, or `ip:port` for tests).
    pub fn at(endpoint: impl Into<String>) -> Arc<Self> {
        Arc::new(FakeLifecycle {
            endpoint: Mutex::new(endpoint.into()),
            token: Mutex::new(String::new()),
            wake_error: Mutex::new(None),
            endpoint_error: Mutex::new(None),
            awake_calls: Mutex::new(Vec::new()),
            activity: AtomicUsize::new(0),
        })
    }
    pub fn failing_wake(endpoint: impl Into<String>, err: Error) -> Arc<Self> {
        let lc = Self::at(endpoint);
        *lc.wake_error.lock().unwrap() = Some(err);
        lc
    }
    /// Every instance resolves to `endpoint` with gateway token `token`.
    pub fn with_token(endpoint: impl Into<String>, token: &str) -> Arc<Self> {
        let lc = Self::at(endpoint);
        *lc.token.lock().unwrap() = token.to_string();
        lc
    }
    pub fn failing_endpoint(err: Error) -> Arc<Self> {
        let lc = Self::at("127.0.0.1");
        *lc.endpoint_error.lock().unwrap() = Some(err);
        lc
    }
    pub fn awake_calls(&self) -> Vec<String> {
        self.awake_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Lifecycle for FakeLifecycle {
    async fn create(&self, _ws: &Workspace, _req: &CreateInstanceRequest) -> Result<Instance> {
        unsupported("create")
    }
    async fn get(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("get")
    }
    async fn list(&self, _workspace_id: &str) -> Result<Vec<Instance>> {
        unsupported("list")
    }
    async fn patch(
        &self,
        _workspace_id: &str,
        _id: &str,
        _req: &PatchInstanceRequest,
    ) -> Result<Instance> {
        unsupported("patch")
    }
    async fn start(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("start")
    }
    async fn stop(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("stop")
    }
    async fn sleep(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("sleep")
    }
    async fn restart(&self, _workspace_id: &str, _id: &str) -> Result<Instance> {
        unsupported("restart")
    }
    async fn delete(&self, _workspace_id: &str, _id: &str) -> Result<()> {
        unsupported("delete")
    }
    async fn resize(
        &self,
        _workspace_id: &str,
        _id: &str,
        _shape: &ShapeRequest,
    ) -> Result<Instance> {
        unsupported("resize")
    }
    async fn update(
        &self,
        _workspace_id: &str,
        _id: &str,
        _template: Option<&str>,
    ) -> Result<Instance> {
        unsupported("update")
    }
    async fn fork(&self, _ws: &Workspace, _id: &str, _req: &ForkRequest) -> Result<Instance> {
        unsupported("fork")
    }
    async fn exec(&self, _workspace_id: &str, _id: &str, _req: &ExecRequest) -> Result<ExecResult> {
        unsupported("exec")
    }
    async fn logs(&self, _workspace_id: &str, _id: &str, _opts: LogsOptions) -> Result<String> {
        unsupported("logs")
    }
    async fn metrics(
        &self,
        _workspace_id: &str,
        _id: &str,
        _hours: u32,
    ) -> Result<Vec<MetricPoint>> {
        unsupported("metrics")
    }
    async fn backup(&self, _workspace_id: &str, _id: &str, _note: &str) -> Result<Backup> {
        unsupported("backup")
    }
    async fn list_backups(&self, _workspace_id: &str, _id: &str) -> Result<Vec<Backup>> {
        unsupported("list_backups")
    }
    async fn restore(&self, _workspace_id: &str, _id: &str, _backup_id: &str) -> Result<Instance> {
        unsupported("restore")
    }

    async fn ensure_awake(&self, id: &str) -> Result<Instance> {
        self.awake_calls.lock().unwrap().push(id.to_string());
        if let Some(e) = self.wake_error.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(Instance {
            id: id.to_string(),
            status: InstanceStatus::Running,
            ..Default::default()
        })
    }
    fn record_activity(&self, _id: &str) {
        self.activity.fetch_add(1, Ordering::SeqCst);
    }
    async fn gateway_credentials(&self, id: &str) -> Result<(String, String)> {
        let endpoint = self.endpoint(id).await?;
        Ok((endpoint, self.token.lock().unwrap().clone()))
    }
    async fn endpoint(&self, _id: &str) -> Result<String> {
        if let Some(e) = self.endpoint_error.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(self.endpoint.lock().unwrap().clone())
    }
}

/// What the fake gateway answers to every request.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// How long to hold the request before answering.
    pub delay: Duration,
}

impl Reply {
    /// `200` with a completed response object.
    pub fn completed() -> Reply {
        let body = serde_json::json!({
            "id": "resp_1", "session_id": "sess_1", "status": "completed",
            "output_text": "done", "created_at": 1_767_225_600_000_i64, "completed_at": 1_767_225_601_000_i64
        });
        Reply {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }
    /// `200` with a failed response object carrying `error`.
    pub fn failed(error: &str) -> Reply {
        let body = serde_json::json!({
            "id": "resp_1", "session_id": "sess_1", "status": "failed", "created_at": 1_767_225_600_000_i64, "error": error
        });
        Reply {
            status: 200,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }
    /// An error envelope with the given HTTP status.
    pub fn error(status: u16, code: &str, message: &str) -> Reply {
        let body = serde_json::json!({ "error": { "code": code, "message": message } });
        Reply {
            status,
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }
    pub fn delayed(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

/// One request the fake gateway received.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: String,
    pub path: String,
    pub content_type: String,
    /// The `X-Warren-Instance-Token` header, when sent.
    pub instance_token: Option<String>,
    pub body: serde_json::Value,
}

/// A minimal HTTP/1.1 server that records requests and answers with a fixed
/// [`Reply`], tracking how many requests it holds at once.
pub struct FakeGateway {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<Received>>>,
    pub max_concurrent: Arc<AtomicUsize>,
    accept: JoinHandle<()>,
}

impl FakeGateway {
    pub async fn spawn(reply: Reply) -> FakeGateway {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let received = Arc::new(Mutex::new(Vec::new()));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let accept = tokio::spawn({
            let (received, max_concurrent) = (received.clone(), max_concurrent.clone());
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    tokio::spawn(serve(
                        stream,
                        reply.clone(),
                        received.clone(),
                        current.clone(),
                        max_concurrent.clone(),
                    ));
                }
            }
        });
        FakeGateway {
            addr,
            received,
            max_concurrent,
            accept,
        }
    }
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
    pub fn requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent.load(Ordering::SeqCst)
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    reply: Reply,
    received: Arc<Mutex<Vec<Received>>>,
    current: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 || buf.len() > (1 << 20) {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut content_type = String::new();
    let mut instance_token = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = value.trim().to_string();
            } else if name.eq_ignore_ascii_case(warren_core::HEADER_INSTANCE_TOKEN) {
                instance_token = Some(value.trim().to_string());
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    received.lock().unwrap().push(Received {
        method,
        path,
        content_type,
        instance_token,
        body: json,
    });

    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
    max_concurrent.fetch_max(now, Ordering::SeqCst);
    tokio::time::sleep(reply.delay).await;
    current.fetch_sub(1, Ordering::SeqCst);

    let reason = match reply.status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reason,
        reply.body.len(),
        reply.body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
