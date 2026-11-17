//! The Sandbox API client.

use std::time::Duration;

use reqwest::Method;
use serde_json::{Map, Value, json};
use warren_core::json::List;
use warren_core::{
    ApiKey, Backup, CreateInstanceRequest, Cron, DEFAULT_EXEC_TIMEOUT_SEC, ExecRequest, ExecResult,
    ForkRequest, Instance, InstanceStatus, MetricPoint, PatchInstanceRequest,
    RegisterTemplateRequest, ShapeRequest, Template,
};

use crate::agent::AgentClient;
use crate::error::SdkError;
use crate::http::{Http, build_url, encode};
use crate::types::*;

/// Where the Sandbox API lives when `WARREN_API_URL` is unset.
pub const DEFAULT_API_URL: &str = "http://api.warren.localhost:7337/v1";
/// Environment variable holding the Sandbox API URL.
pub const ENV_API_URL: &str = "WARREN_API_URL";
/// Environment variable holding the workspace API key.
pub const ENV_API_KEY: &str = "WARREN_API_KEY";
/// How often [`Client::wait_for_status`] polls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Client for the Sandbox API (`/v1` on the API host).
///
/// Every request carries the key as both `Authorization: Bearer` and
/// `x-warren-key`. Cloning is cheap: the HTTP client is reference counted.
#[derive(Debug, Clone)]
pub struct Client {
    /// Base URL ending in `/v1`, no trailing slash.
    pub api_url: String,
    /// Workspace API key; empty for unauthenticated calls such as signup.
    pub key: String,
    pub http: reqwest::Client,
}

impl Client {
    /// A client with a default HTTP client (rustls, 10 s connect timeout).
    /// `api_url` may omit the `/v1` suffix; it is added when missing.
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>) -> Client {
        Self::with_http(api_url, api_key, crate::http_client())
    }

    /// A client over a caller-provided `reqwest::Client` (see
    /// [`crate::client_with_resolve`] for tests).
    pub fn with_http(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        http: reqwest::Client,
    ) -> Client {
        Client {
            api_url: normalize_api_url(&api_url.into()),
            key: api_key.into().trim().to_string(),
            http,
        }
    }

    /// From `WARREN_API_URL` (default [`DEFAULT_API_URL`]) and `WARREN_API_KEY`
    /// (default empty).
    pub fn from_env() -> Client {
        Self::from_vars(
            std::env::var(ENV_API_URL).ok(),
            std::env::var(ENV_API_KEY).ok(),
        )
    }

    /// [`from_env`](Self::from_env) with the values passed in.
    pub fn from_vars(api_url: Option<String>, api_key: Option<String>) -> Client {
        let url = api_url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());
        Self::new(url, api_key.unwrap_or_default())
    }

    /// Same key and HTTP client, for a different key.
    pub fn with_key(&self, api_key: impl Into<String>) -> Client {
        Client {
            api_url: self.api_url.clone(),
            key: api_key.into().trim().to_string(),
            http: self.http.clone(),
        }
    }

    /// An Agent API client for the instance at `instance_url`, sharing this
    /// client's key and HTTP client.
    pub fn agent(&self, instance_url: impl Into<String>) -> AgentClient {
        AgentClient::with_http(instance_url, self.key.clone(), self.http.clone())
    }

    /// An Agent API client for `inst` (using its `url`).
    pub fn agent_for(&self, inst: &Instance) -> AgentClient {
        self.agent(inst.url.clone())
    }

    fn http(&self) -> Http<'_> {
        Http {
            http: &self.http,
            key: &self.key,
        }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> String {
        build_url(&self.api_url, path, query)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, SdkError> {
        let h = self.http();
        h.json(h.json_request(Method::GET, &self.url(path, query)))
            .await
    }

    async fn list<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<T>, SdkError> {
        self.get::<List<T>>(path, query).await.map(|l| l.data)
    }

    async fn send_json<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T, SdkError> {
        let h = self.http();
        h.json(h.json_request(method, &self.url(path, &[])).json(body))
            .await
    }

    async fn post<T: serde::de::DeserializeOwned, B: serde::Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SdkError> {
        self.send_json(Method::POST, path, body).await
    }

    async fn delete<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, SdkError> {
        let h = self.http();
        h.json(h.json_request(Method::DELETE, &self.url(path, &[])))
            .await
    }

    // ----- health, version, signup -------------------------------------------------

    /// `GET /v1/health`. No auth required.
    pub async fn health(&self) -> Result<HealthResponse, SdkError> {
        self.get("/health", &[]).await
    }

    /// `GET /v1/version`. No auth required.
    pub async fn version(&self) -> Result<String, SdkError> {
        self.get::<VersionResponse>("/version", &[])
            .await
            .map(|v| v.version)
    }

    /// `POST /v1/signup`: a new workspace and its first key. Sent without
    /// credentials; the client's own key is not needed or used.
    pub async fn signup(&self, name: Option<&str>) -> Result<SignupResponse, SdkError> {
        let anon = Http {
            http: &self.http,
            key: "",
        };
        let mut body = Map::new();
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            body.insert("name".into(), Value::String(n.to_string()));
        }
        anon.json(
            anon.json_request(Method::POST, &self.url("/signup", &[]))
                .json(&Value::Object(body)),
        )
        .await
    }

    // ----- workspace, keys --------------------------------------------------------

    /// `GET /v1/workspace`.
    pub async fn workspace(&self) -> Result<WorkspaceResponse, SdkError> {
        self.get("/workspace", &[]).await
    }

    /// `GET /v1/api-keys`.
    pub async fn api_keys(&self) -> Result<Vec<ApiKey>, SdkError> {
        self.list("/api-keys", &[]).await
    }

    /// `POST /v1/api-keys`. `expires_at` is unix seconds; `None` never expires.
    pub async fn create_api_key(
        &self,
        name: Option<&str>,
        expires_at: Option<i64>,
    ) -> Result<CreateApiKeyResponse, SdkError> {
        let mut body = Map::new();
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            body.insert("name".into(), Value::String(n.to_string()));
        }
        if let Some(t) = expires_at.filter(|t| *t > 0) {
            body.insert("expires_at".into(), json!(t));
        }
        self.post("/api-keys", &Value::Object(body)).await
    }

    /// `DELETE /v1/api-keys/{id}`.
    pub async fn delete_api_key(&self, id: &str) -> Result<DeletedKey, SdkError> {
        self.delete(&format!("/api-keys/{}", encode(id))).await
    }

    // ----- templates ----------------------------------------------------------------

    /// `GET /v1/templates`.
    pub async fn templates(&self) -> Result<Vec<Template>, SdkError> {
        self.list("/templates", &[]).await
    }

    /// `GET /v1/templates/{name}`; `name@<revision>` pins a workspace revision.
    pub async fn template(&self, name: &str) -> Result<Template, SdkError> {
        self.get(&format!("/templates/{}", encode(name)), &[]).await
    }

    /// `POST /v1/templates`.
    pub async fn register_template(
        &self,
        req: &RegisterTemplateRequest,
    ) -> Result<Template, SdkError> {
        self.post("/templates", req).await
    }

    /// `DELETE /v1/templates/{name}`.
    pub async fn delete_template(&self, name: &str) -> Result<DeletedTemplate, SdkError> {
        self.delete(&format!("/templates/{}", encode(name))).await
    }

    // ----- instances ----------------------------------------------------------------

    /// `GET /v1/instances`.
    pub async fn instances(&self) -> Result<Vec<Instance>, SdkError> {
        self.list("/instances", &[]).await
    }

    /// `POST /v1/instances`. Returns the instance in `provisioning`; use
    /// [`wait_for_status`](Self::wait_for_status) to wait for `running`.
    pub async fn create_instance(&self, req: &CreateInstanceRequest) -> Result<Instance, SdkError> {
        self.post("/instances", req).await
    }

    /// `GET /v1/instances/{id}`.
    pub async fn instance(&self, id: &str) -> Result<Instance, SdkError> {
        self.get(&instance_path(id, ""), &[]).await
    }

    /// `PATCH /v1/instances/{id}`.
    pub async fn patch_instance(
        &self,
        id: &str,
        req: &PatchInstanceRequest,
    ) -> Result<Instance, SdkError> {
        self.send_json(Method::PATCH, &instance_path(id, ""), req)
            .await
    }

    /// `DELETE /v1/instances/{id}` -> status `deleting`.
    pub async fn delete_instance(&self, id: &str) -> Result<StatusResponse, SdkError> {
        self.delete(&instance_path(id, "")).await
    }

    /// `POST /v1/instances/{id}/stop`.
    pub async fn stop(&self, id: &str) -> Result<StatusResponse, SdkError> {
        self.post(&instance_path(id, "/stop"), &json!({})).await
    }

    /// `POST /v1/instances/{id}/start`.
    pub async fn start(&self, id: &str) -> Result<StatusResponse, SdkError> {
        self.post(&instance_path(id, "/start"), &json!({})).await
    }

    /// `POST /v1/instances/{id}/sleep`.
    pub async fn sleep(&self, id: &str) -> Result<StatusResponse, SdkError> {
        self.post(&instance_path(id, "/sleep"), &json!({})).await
    }

    /// `POST /v1/instances/{id}/restart`.
    pub async fn restart(&self, id: &str) -> Result<StatusResponse, SdkError> {
        self.post(&instance_path(id, "/restart"), &json!({})).await
    }

    /// `POST /v1/instances/{id}/resize`. Disk may only grow.
    pub async fn resize(&self, id: &str, shape: &ShapeRequest) -> Result<ResizeResponse, SdkError> {
        self.post(&instance_path(id, "/resize"), shape).await
    }

    /// `POST /v1/instances/{id}/update`; `template` is `name` or `name@rev`,
    /// `None` re-pulls the current template.
    pub async fn update(
        &self,
        id: &str,
        template: Option<&str>,
    ) -> Result<UpdateResponse, SdkError> {
        let mut body = Map::new();
        if let Some(t) = template.filter(|t| !t.is_empty()) {
            body.insert("template".into(), Value::String(t.to_string()));
        }
        self.post(&instance_path(id, "/update"), &Value::Object(body))
            .await
    }

    /// `POST /v1/instances/{id}/fork` -> the new instance.
    pub async fn fork(&self, id: &str, req: &ForkRequest) -> Result<Instance, SdkError> {
        self.post(&instance_path(id, "/fork"), req).await
    }

    /// `POST /v1/instances/{id}/exec`. The HTTP timeout is the command timeout
    /// (default 15 minutes) plus a margin, so long commands are not cut short.
    pub async fn exec(&self, id: &str, req: &ExecRequest) -> Result<ExecResult, SdkError> {
        let secs = if req.timeout_seconds > 0 {
            req.timeout_seconds
        } else {
            DEFAULT_EXEC_TIMEOUT_SEC
        };
        let h = self.http();
        let rb = h
            .json_request(Method::POST, &self.url(&instance_path(id, "/exec"), &[]))
            .timeout(Duration::from_secs(u64::from(secs) + 60))
            .json(req);
        h.json(rb).await
    }

    /// `GET /v1/instances/{id}/logs?tail=&since=`. Zero means "not set".
    pub async fn logs(&self, id: &str, tail: u32, since: i64) -> Result<String, SdkError> {
        let mut q = Vec::new();
        if tail > 0 {
            q.push(("tail", tail.to_string()));
        }
        if since > 0 {
            q.push(("since", since.to_string()));
        }
        self.get::<LogsResponse>(&instance_path(id, "/logs"), &q)
            .await
            .map(|l| l.logs)
    }

    /// `GET /v1/instances/{id}/metrics?hours=`. Zero lets the server choose.
    pub async fn metrics(&self, id: &str, hours: u32) -> Result<Vec<MetricPoint>, SdkError> {
        let mut q = Vec::new();
        if hours > 0 {
            q.push(("hours", hours.to_string()));
        }
        self.list(&instance_path(id, "/metrics"), &q).await
    }

    /// `GET /v1/instances/{id}/signed-url?port=&ttl=&path=`. Port 0 means the
    /// default port, TTL 0 the server default, an empty path `/`.
    pub async fn signed_url(
        &self,
        id: &str,
        port: u16,
        ttl: i64,
        path: &str,
    ) -> Result<SignedUrl, SdkError> {
        let mut q = Vec::new();
        if port > 0 {
            q.push(("port", port.to_string()));
        }
        if ttl > 0 {
            q.push(("ttl", ttl.to_string()));
        }
        if !path.is_empty() {
            q.push(("path", path.to_string()));
        }
        self.get(&instance_path(id, "/signed-url"), &q).await
    }

    // ----- backups ------------------------------------------------------------------

    /// `GET /v1/instances/{id}/backups`.
    pub async fn backups(&self, id: &str) -> Result<Vec<Backup>, SdkError> {
        self.list(&instance_path(id, "/backups"), &[]).await
    }

    /// `POST /v1/instances/{id}/backups`.
    pub async fn create_backup(&self, id: &str, note: &str) -> Result<Backup, SdkError> {
        let mut body = Map::new();
        if !note.is_empty() {
            body.insert("note".into(), Value::String(note.to_string()));
        }
        self.post(&instance_path(id, "/backups"), &Value::Object(body))
            .await
    }

    /// `POST /v1/instances/{id}/restore` -> the restored instance.
    pub async fn restore(&self, id: &str, backup_id: &str) -> Result<Instance, SdkError> {
        self.post(
            &instance_path(id, "/restore"),
            &json!({ "backup_id": backup_id }),
        )
        .await
    }

    // ----- crons --------------------------------------------------------------------

    /// `GET /v1/instances/{id}/crons`.
    pub async fn crons(&self, id: &str) -> Result<Vec<Cron>, SdkError> {
        self.list(&instance_path(id, "/crons"), &[]).await
    }

    /// `POST /v1/instances/{id}/crons`. Agent templates only.
    pub async fn create_cron(&self, id: &str, req: &CronRequest) -> Result<Cron, SdkError> {
        self.post(&instance_path(id, "/crons"), req).await
    }

    /// `GET /v1/instances/{id}/crons/{cid}`.
    pub async fn cron(&self, id: &str, cron_id: &str) -> Result<Cron, SdkError> {
        self.get(&cron_path(id, cron_id), &[]).await
    }

    /// `PATCH /v1/instances/{id}/crons/{cid}`.
    pub async fn patch_cron(
        &self,
        id: &str,
        cron_id: &str,
        req: &CronPatchRequest,
    ) -> Result<Cron, SdkError> {
        self.send_json(Method::PATCH, &cron_path(id, cron_id), req)
            .await
    }

    /// `DELETE /v1/instances/{id}/crons/{cid}`. The body is not specified by
    /// the contract and is ignored.
    pub async fn delete_cron(&self, id: &str, cron_id: &str) -> Result<(), SdkError> {
        let h = self.http();
        h.empty(h.json_request(Method::DELETE, &self.url(&cron_path(id, cron_id), &[])))
            .await
    }

    // ----- waiting ------------------------------------------------------------------

    /// Poll `GET /v1/instances/{id}` every [`POLL_INTERVAL`] until the instance
    /// reports `status` and return it. Gives up with a `timeout` API error after
    /// `timeout`; returns `conflict` early when the instance reaches `failed`
    /// (unless that is the target) and `not_found` when it is deleted. Waiting
    /// for `deleted` succeeds once the server answers `404`; the returned
    /// instance then carries only the id and the status.
    pub async fn wait_for_status(
        &self,
        id: &str,
        status: InstanceStatus,
        timeout: Duration,
    ) -> Result<Instance, SdkError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let last = match self.instance(id).await {
                Ok(inst) if inst.status == status => return Ok(inst),
                Ok(inst) if inst.status == InstanceStatus::Failed => {
                    let reason = if inst.status_reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", inst.status_reason)
                    };
                    return Err(SdkError::Api(warren_core::Error::conflict(format!(
                        "instance {id} failed{reason}"
                    ))));
                }
                Ok(inst) if inst.status == InstanceStatus::Deleted => {
                    return Err(SdkError::Api(warren_core::Error::not_found("instance")));
                }
                Ok(inst) => inst.status,
                Err(e) if e.is_not_found() && status == InstanceStatus::Deleted => {
                    return Ok(Instance {
                        id: id.to_string(),
                        status: InstanceStatus::Deleted,
                        ..Default::default()
                    });
                }
                Err(e) => return Err(e),
            };
            let now = tokio::time::Instant::now();
            if now + POLL_INTERVAL > deadline {
                return Err(SdkError::Api(warren_core::Error::timeout(format!(
                    "instance {id} did not reach {status} within {}s (last status: {last})",
                    timeout.as_secs_f64()
                ))));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// Trim, drop trailing slashes and make sure the URL ends in `/v1`.
pub(crate) fn normalize_api_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT_API_URL.to_string();
    }
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn instance_path(id: &str, suffix: &str) -> String {
    format!("/instances/{}{suffix}", encode(id))
}

fn cron_path(id: &str, cron_id: &str) -> String {
    format!("/instances/{}/crons/{}", encode(id), encode(cron_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_api_urls() {
        assert_eq!(
            normalize_api_url("http://api.warren.localhost:7337/v1"),
            DEFAULT_API_URL
        );
        assert_eq!(
            normalize_api_url("http://api.warren.localhost:7337/v1/"),
            DEFAULT_API_URL
        );
        assert_eq!(
            normalize_api_url("http://api.warren.localhost:7337"),
            DEFAULT_API_URL
        );
        assert_eq!(
            normalize_api_url("  http://api.warren.localhost:7337/  "),
            DEFAULT_API_URL
        );
        assert_eq!(normalize_api_url(""), DEFAULT_API_URL);
    }

    #[test]
    fn from_vars_applies_defaults() {
        let c = Client::from_vars(None, None);
        assert_eq!(c.api_url, DEFAULT_API_URL);
        assert_eq!(c.key, "");
        let c = Client::from_vars(Some("http://h:1".into()), Some(" sk_live_x ".into()));
        assert_eq!(c.api_url, "http://h:1/v1");
        assert_eq!(c.key, "sk_live_x");
        let c = Client::from_vars(Some("  ".into()), None);
        assert_eq!(c.api_url, DEFAULT_API_URL);
    }

    #[test]
    fn paths_are_encoded() {
        assert_eq!(
            instance_path("ab12cd34ef", "/exec"),
            "/instances/ab12cd34ef/exec"
        );
        assert_eq!(
            cron_path("ab12cd34ef", "c/1"),
            "/instances/ab12cd34ef/crons/c%2F1"
        );
    }
}
