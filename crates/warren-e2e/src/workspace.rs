//! A workspace's view of the daemon: the SDK client bound to its key plus the
//! raw requests, waits and cleanup the tests need around it.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use reqwest::Method;
use serde_json::Value;
use warren_core::{CreateInstanceRequest, Instance, InstanceStatus, Result};
use warren_sdk::{AgentClient, Client, SignupResponse};

use crate::{
    Daemon, Edge, REQUEST_TIMEOUT, ensure,
    util::{api_error, fail, preview_bytes, transport},
};

/// How long a fresh instance may take to reach `running` (image start, gateway boot).
pub const RUNNING_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a transition such as stop, start, restore or delete may take.
pub const TRANSITION_TIMEOUT: Duration = Duration::from_secs(120);

/// One workspace: its SDK client, its daemon and helpers.
#[derive(Clone)]
pub struct Workspace {
    daemon: Arc<Daemon>,
    sdk: Client,
    workspace_id: String,
}

impl Workspace {
    /// `POST /v1/signup` through the SDK: a new workspace with the free credit.
    /// Returns the workspace client and the raw signup answer.
    pub async fn signup(daemon: Arc<Daemon>, name: &str) -> Result<(Workspace, SignupResponse)> {
        let anon = Client::with_http(daemon.api_url(), "", daemon.client(&daemon.api_host())?);
        let signup = anon.signup(Some(name)).await?;
        ensure!(
            signup.api_key.starts_with(warren_core::ids::KEY_PREFIX),
            "signup returned an odd key: {}",
            signup.api_key
        );
        let sdk = anon.with_key(signup.api_key.clone());
        let workspace_id = signup.workspace.id.clone();
        Ok((
            Workspace {
                daemon,
                sdk,
                workspace_id,
            },
            signup,
        ))
    }

    /// A workspace client for an existing key.
    pub fn with_key(daemon: Arc<Daemon>, api_key: &str, workspace_id: &str) -> Result<Workspace> {
        let sdk = Client::with_http(
            daemon.api_url(),
            api_key,
            daemon.client(&daemon.api_host())?,
        );
        Ok(Workspace {
            daemon,
            sdk,
            workspace_id: workspace_id.to_string(),
        })
    }

    pub fn daemon(&self) -> &Arc<Daemon> {
        &self.daemon
    }

    /// The SDK client for the Sandbox API, bound to this workspace's key.
    pub fn sdk(&self) -> &Client {
        &self.sdk
    }

    pub fn api_key(&self) -> &str {
        &self.sdk.key
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Raw requests to an instance's URLs with this workspace's key.
    pub fn edge(&self, id: &str) -> Edge {
        Edge::new(self.daemon.clone(), id, self.api_key())
    }

    /// The SDK's Agent API client for an instance, through the edge on the
    /// default port, with DNS pinned to the daemon.
    pub fn agent(&self, id: &str) -> Result<AgentClient> {
        let url = self.daemon.instance_url(id, 0);
        let http = self.daemon.client_for_url(&url)?;
        Ok(AgentClient::with_http(url, self.api_key(), http))
    }

    /// Send a Sandbox API request with an explicit key (empty for none) as a
    /// Bearer token, outside the SDK. Returns (status, body) for 2xx and an
    /// `Error` carrying the daemon's code and status otherwise. For negative
    /// tests such as bad keys or unknown JSON fields.
    pub async fn call_with_key(
        &self,
        key: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let client = self.daemon.client(&self.daemon.api_host())?;
        let url = format!("{}{path}", self.daemon.api_url());
        let mut req = client
            .request(method.clone(), &url)
            .timeout(REQUEST_TIMEOUT);
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| transport(&format!("{method} {url}"), e))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| transport("read response body", e))?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &bytes));
        }
        if bytes.is_empty() {
            return Ok((status, Value::Null));
        }
        let v = serde_json::from_slice(&bytes).map_err(|e| {
            fail(format!(
                "invalid JSON in a {status} response: {e}: {}",
                preview_bytes(&bytes)
            ))
        })?;
        Ok((status, v))
    }

    /// Wait until the instance reports `target` (see `Client::wait_for_status`).
    pub async fn wait_for_status(
        &self,
        id: &str,
        target: InstanceStatus,
        timeout: Duration,
    ) -> Result<Instance> {
        Ok(self.sdk.wait_for_status(id, target, timeout).await?)
    }

    /// Wait up to [`RUNNING_TIMEOUT`] for `running`.
    pub async fn wait_running(&self, id: &str) -> Result<Instance> {
        self.wait_for_status(id, InstanceStatus::Running, RUNNING_TIMEOUT)
            .await
    }

    /// Wait until the instance is gone (`404` or `deleted`).
    pub async fn wait_deleted(&self, id: &str, timeout: Duration) -> Result<()> {
        self.wait_for_status(id, InstanceStatus::Deleted, timeout)
            .await
            .map(|_| ())
    }

    /// Create an instance from `template` with `env` and wait for `running`.
    pub async fn create_and_wait(&self, template: &str, env: &[(&str, &str)]) -> Result<Instance> {
        let env: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let req = CreateInstanceRequest {
            template: Some(template.to_string()),
            env: if env.is_empty() { None } else { Some(env) },
            ..Default::default()
        };
        let inst = self.sdk.create_instance(&req).await?;
        ensure!(
            inst.status.awake(),
            "create should answer with provisioning, got {} for {}",
            inst.status,
            inst.id
        );
        self.wait_running(&inst.id).await
    }

    /// Delete an instance and wait until it is gone.
    pub async fn delete_and_wait(&self, id: &str) -> Result<()> {
        let answer = self.sdk.delete_instance(id).await?;
        ensure!(
            matches!(
                answer.status,
                InstanceStatus::Deleting | InstanceStatus::Deleted
            ),
            "delete should answer deleting: {answer:?}"
        );
        self.wait_deleted(id, TRANSITION_TIMEOUT).await
    }

    /// Best-effort removal of every instance in the workspace; problems are
    /// printed, never returned, so cleanup can run after a failed test.
    pub async fn cleanup(&self) {
        let instances = match self.sdk.instances().await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[e2e] cleanup: cannot list instances: {e}");
                return;
            }
        };
        for inst in &instances {
            if matches!(
                inst.status,
                InstanceStatus::Deleting | InstanceStatus::Deleted
            ) {
                continue;
            }
            // Transitional states can refuse a delete for a moment; retry a few times.
            for attempt in 1..=5 {
                match self.sdk.delete_instance(&inst.id).await {
                    Ok(_) => break,
                    Err(e) if e.is_not_found() => break,
                    Err(e) => {
                        eprintln!(
                            "[e2e] cleanup: delete {} attempt {attempt} failed: {e}",
                            inst.id
                        );
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }
        for inst in &instances {
            if let Err(e) = self.wait_deleted(&inst.id, Duration::from_secs(90)).await {
                eprintln!("[e2e] cleanup: {}: {e}", inst.id);
            }
        }
    }
}
