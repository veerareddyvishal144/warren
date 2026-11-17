//! The one module that talks to `warren-sdk`.
//!
//! Every method returns the answer as JSON so `--json` prints what the daemon
//! sent (lists are re-wrapped in the `{"data": [...]}` envelope) and the
//! tables are rendered from the same value. SDK errors become
//! `warren_core::Error` through the SDK's `From` impl and then `Failure::Api`.

use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use warren_cli::Failure;
use warren_cli::config::Settings;
use warren_cli::output::list_envelope;
use warren_core::agentapi::ResponsesRequest;
use warren_core::{
    CreateInstanceRequest, Error, ExecRequest, ExecResult, ForkRequest, RegisterTemplateRequest,
    ShapeRequest,
};
use warren_sdk::{AgentClient, Client, CronRequest, SdkError};

/// One named event of a streamed agent response.
#[derive(Debug, Clone)]
pub struct ChatEvent {
    /// `response.created`, `response.output_text.delta`, `response.tool_call`,
    /// `response.completed` or `response.failed`.
    pub name: String,
    pub data: Value,
}

/// The events of one streamed response, in order.
pub type ChatEvents = Pin<Box<dyn Stream<Item = Result<ChatEvent, Failure>>>>;

type R = Result<Value, Failure>;

/// Thin wrapper over the SDK's Sandbox API and Agent API clients.
pub struct Api {
    client: Client,
}

fn fail(e: SdkError) -> Failure {
    Failure::Api(Error::from(e))
}

fn encode(r: serde_json::Result<Value>) -> R {
    r.map_err(|e| Failure::local(format!("encoding the response: {e}")))
}

/// Wrap an array in the wire list envelope.
fn envelope(v: Value) -> Value {
    match v {
        Value::Array(items) => list_envelope(items),
        other => other,
    }
}

/// Run an SDK call and return its answer as a JSON value.
macro_rules! call {
    ($e:expr) => {{
        let out = $e.await.map_err(fail)?;
        encode(serde_json::to_value(&out))
    }};
}

/// Run an SDK call returning a `Vec` and wrap it in `{"data": [...]}`.
macro_rules! list {
    ($e:expr) => {{
        let out = $e.await.map_err(fail)?;
        encode(serde_json::to_value(&out)).map(envelope)
    }};
}

impl Api {
    /// A client for `settings`; an empty key is fine for health and signup.
    pub fn new(settings: &Settings) -> Result<Api, Failure> {
        Ok(Api {
            client: Client::new(
                settings.api_url.clone(),
                settings.api_key.clone().unwrap_or_default(),
            ),
        })
    }

    // ----- health, signup, workspace, keys ----------------------------------------

    pub async fn health(&self) -> R {
        call!(self.client.health())
    }

    pub async fn signup(&self, name: Option<&str>) -> R {
        call!(self.client.signup(name))
    }

    pub async fn workspace(&self) -> R {
        call!(self.client.workspace())
    }

    pub async fn list_api_keys(&self) -> R {
        list!(self.client.api_keys())
    }

    pub async fn create_api_key(&self, name: Option<&str>) -> R {
        call!(self.client.create_api_key(name, None))
    }

    pub async fn delete_api_key(&self, id: &str) -> R {
        call!(self.client.delete_api_key(id))
    }

    // ----- templates ------------------------------------------------------------------

    pub async fn list_templates(&self) -> R {
        list!(self.client.templates())
    }

    pub async fn get_template(&self, name: &str) -> R {
        call!(self.client.template(name))
    }

    pub async fn register_template(&self, req: &RegisterTemplateRequest) -> R {
        call!(self.client.register_template(req))
    }

    pub async fn delete_template(&self, name: &str) -> R {
        call!(self.client.delete_template(name))
    }

    // ----- instances ------------------------------------------------------------------

    pub async fn list_instances(&self) -> R {
        list!(self.client.instances())
    }

    pub async fn create_instance(&self, req: &CreateInstanceRequest) -> R {
        call!(self.client.create_instance(req))
    }

    pub async fn get_instance(&self, id: &str) -> R {
        call!(self.client.instance(id))
    }

    pub async fn delete_instance(&self, id: &str) -> R {
        call!(self.client.delete_instance(id))
    }

    pub async fn stop_instance(&self, id: &str) -> R {
        call!(self.client.stop(id))
    }

    pub async fn start_instance(&self, id: &str) -> R {
        call!(self.client.start(id))
    }

    pub async fn sleep_instance(&self, id: &str) -> R {
        call!(self.client.sleep(id))
    }

    pub async fn restart_instance(&self, id: &str) -> R {
        call!(self.client.restart(id))
    }

    pub async fn resize_instance(&self, id: &str, shape: &ShapeRequest) -> R {
        call!(self.client.resize(id, shape))
    }

    pub async fn update_instance(&self, id: &str, template: Option<&str>) -> R {
        call!(self.client.update(id, template))
    }

    pub async fn fork_instance(&self, id: &str, req: &ForkRequest) -> R {
        call!(self.client.fork(id, req))
    }

    /// Typed, because the exit code and the two output streams are used directly.
    pub async fn exec(&self, id: &str, req: &ExecRequest) -> Result<ExecResult, Failure> {
        self.client.exec(id, req).await.map_err(fail)
    }

    /// `{ "logs": "..." }`, as on the wire.
    pub async fn logs(&self, id: &str, tail: Option<u32>) -> R {
        let logs = self
            .client
            .logs(id, tail.unwrap_or(0), 0)
            .await
            .map_err(fail)?;
        Ok(json!({ "logs": logs }))
    }

    pub async fn metrics(&self, id: &str, hours: Option<u32>) -> R {
        list!(self.client.metrics(id, hours.unwrap_or(0)))
    }

    /// `{ "url", "expires_at" }`; `None` port and TTL take the server defaults.
    pub async fn signed_url(&self, id: &str, port: Option<u16>, ttl: Option<u32>) -> R {
        call!(
            self.client
                .signed_url(id, port.unwrap_or(0), ttl.map(i64::from).unwrap_or(0), "")
        )
    }

    // ----- backups and crons ----------------------------------------------------------

    pub async fn list_backups(&self, id: &str) -> R {
        list!(self.client.backups(id))
    }

    pub async fn create_backup(&self, id: &str, note: Option<&str>) -> R {
        call!(self.client.create_backup(id, note.unwrap_or("")))
    }

    pub async fn restore_backup(&self, id: &str, backup_id: &str) -> R {
        call!(self.client.restore(id, backup_id))
    }

    pub async fn list_crons(&self, id: &str) -> R {
        list!(self.client.crons(id))
    }

    pub async fn create_cron(
        &self,
        id: &str,
        prompt: &str,
        schedule: &str,
        tz: Option<&str>,
        name: Option<&str>,
    ) -> R {
        let req = CronRequest {
            prompt: prompt.to_string(),
            schedule: schedule.to_string(),
            timezone: tz.map(str::to_string),
            name: name.map(str::to_string),
            enabled: None,
        };
        call!(self.client.create_cron(id, &req))
    }

    /// The contract leaves the delete body unspecified; a `{ "id", "deleted" }`
    /// object is synthesised for `--json`.
    pub async fn delete_cron(&self, id: &str, cron_id: &str) -> R {
        self.client.delete_cron(id, cron_id).await.map_err(fail)?;
        Ok(json!({ "id": cron_id, "instance_id": id, "deleted": true }))
    }

    // ----- Agent API ------------------------------------------------------------------

    /// The gateway client for an instance, at the URL the daemon reports.
    async fn agent(&self, id: &str) -> Result<AgentClient, Failure> {
        let inst = self.client.instance(id).await.map_err(fail)?;
        if inst.url.is_empty() {
            return Err(Failure::Api(Error::conflict(format!(
                "instance {id} has no URL yet (status {})",
                inst.status
            ))));
        }
        Ok(self.client.agent_for(&inst))
    }

    /// One complete, non-streamed response.
    pub async fn chat(&self, id: &str, req: &ResponsesRequest) -> R {
        let agent = self.agent(id).await?;
        call!(agent.respond(req))
    }

    /// The events of a streamed response.
    pub async fn chat_stream(
        &self,
        id: &str,
        req: &ResponsesRequest,
    ) -> Result<ChatEvents, Failure> {
        let agent = self.agent(id).await?;
        let events = agent.stream(req).map(|r| {
            r.map(|ev| ChatEvent {
                name: ev.event,
                data: ev.data,
            })
            .map_err(fail)
        });
        Ok(Box::pin(events))
    }

    pub async fn files_list(&self, id: &str, path: &str) -> R {
        let agent = self.agent(id).await?;
        call!(agent.list_files(path))
    }

    pub async fn files_read(&self, id: &str, path: &str) -> Result<Vec<u8>, Failure> {
        let agent = self.agent(id).await?;
        agent.read_file(path).await.map_err(fail)
    }

    pub async fn files_write(&self, id: &str, path: &str, bytes: Vec<u8>) -> R {
        let agent = self.agent(id).await?;
        call!(agent.write_file(path, bytes))
    }

    pub async fn files_delete(&self, id: &str, path: &str) -> R {
        let agent = self.agent(id).await?;
        call!(agent.delete_file(path))
    }

    pub async fn files_download(&self, id: &str, path: &str) -> Result<Vec<u8>, Failure> {
        let agent = self.agent(id).await?;
        agent.download(path).await.map_err(fail)
    }
}
