//! The MCP server: tools, the `warren://instances` resource and server info.
//!
//! Every tool returns JSON (pretty text plus `structuredContent`). Warren
//! failures become tool errors (`isError: true`) whose text is
//! `<code>: <message>` so the model can branch on the API code.

use std::time::Duration;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::IntoCallToolResult, wrapper::Parameters},
    model::{
        CallToolResponse, CallToolResult, ContentBlock, Implementation, ListResourcesResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Serialize;
use serde_json::json;
use warren_core::{
    Error, Instance, InstanceStatus, MAX_EXEC_OUTPUT_BYTES, Result as WarrenResult,
    agentapi::ResponseStatus, json::List,
};

use crate::{
    args::{
        ChatArgs, CreateInstanceArgs, ExecArgs, InstanceIdArgs, ListFilesArgs, ReadFileArgs,
        SignedUrlArgs, WriteFileArgs, file_path,
    },
    backend::Backend,
};

/// URI of the resource that lists instances.
pub const INSTANCES_RESOURCE_URI: &str = "warren://instances";
/// How long `create_instance` waits for `running`.
pub const DEFAULT_CREATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Delay between status polls while waiting.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// `read_file` returns at most this many bytes (the exec output cap, 1 MiB).
pub const MAX_READ_BYTES: usize = MAX_EXEC_OUTPUT_BYTES;
/// Tool error code when a new instance ends in `failed` instead of `running`.
pub const CODE_INSTANCE_FAILED: &str = "instance_failed";
/// Tool error code when the agent inside the instance reports a failed turn.
pub const CODE_RESPONSE_FAILED: &str = "response_failed";

const INSTRUCTIONS: &str = "Warren gives you persistent computers (instances; Linux, or macOS with type mac) reachable over HTTPS. \
Typical flow: create_instance (returns once the instance is running) -> exec / write_file / read_file / list_files \
or chat (agent templates only) -> sleep_instance or stop_instance when idle -> delete_instance when done. \
Instance ids are 10 lowercase alphanumerics (e.g. ab12cd34ef). File paths are relative to the instance home. \
Sleeping instances wake on the first exec or request; stopped instances must be started explicitly. \
Tool errors read `<code>: <message>` using the Warren API codes (invalid_request, not_found, conflict, \
instance_limit_reached, not_supported, try_again, timeout, internal). \
The resource warren://instances lists every instance as JSON.";

/// A tool failure the model should see: the Warren error code and message.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError(pub Error);

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        ToolError(e)
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> std::result::Result<CallToolResponse, ErrorData> {
        Ok(error_result(&self.0).into())
    }
}

type ToolOutcome = std::result::Result<CallToolResult, ToolError>;

/// Render a Warren error as a tool error: `code: message` text plus the error
/// object (`{"error": {code, message, details}}`) as structured content.
/// Details are kept: this server runs on the user's machine with their key.
pub fn error_result(e: &Error) -> CallToolResult {
    let mut text = format!("{}: {}", e.code, e.message);
    if let Some(details) = &e.details {
        text.push_str("\ndetails: ");
        text.push_str(&details.to_string());
    }
    let mut result = CallToolResult::error(vec![ContentBlock::text(text)]);
    result.structured_content = Some(json!({ "error": e }));
    result
}

/// Render a successful value as pretty JSON text plus structured content.
pub fn ok_json<T: Serialize>(value: &T) -> WarrenResult<CallToolResult> {
    let value = serde_json::to_value(value)?;
    let text = serde_json::to_string_pretty(&value)?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    Ok(result)
}

/// Standard base64 with padding; small enough to keep out of the dependency list.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What `read_file` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileContent {
    /// Path as requested.
    pub path: String,
    /// Size of the whole file in bytes.
    pub size: usize,
    /// True when `content` holds only the first [`MAX_READ_BYTES`] bytes.
    pub truncated: bool,
    /// `"utf-8"` for text (content is the text) or `"base64"` for binary data.
    pub encoding: &'static str,
    /// The file body in the given encoding.
    pub content: String,
}

impl FileContent {
    /// Decode raw bytes, capping at [`MAX_READ_BYTES`].
    pub fn from_bytes(path: &str, mut bytes: Vec<u8>) -> Self {
        let size = bytes.len();
        let truncated = size > MAX_READ_BYTES;
        if truncated {
            bytes.truncate(MAX_READ_BYTES);
        }
        let (encoding, content) = match String::from_utf8(bytes) {
            Ok(text) => ("utf-8", text),
            // The cap fell inside a multi-byte character: drop the partial tail.
            Err(e) if truncated && e.utf8_error().error_len().is_none() => {
                let valid = e.utf8_error().valid_up_to();
                let mut bytes = e.into_bytes();
                bytes.truncate(valid);
                ("utf-8", String::from_utf8(bytes).unwrap_or_default())
            }
            Err(e) => ("base64", base64_encode(e.as_bytes())),
        };
        FileContent {
            path: path.to_string(),
            size,
            truncated,
            encoding,
            content,
        }
    }
}

/// What `chat` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatReply {
    /// The agent's answer.
    pub output_text: String,
    /// Pass back to `chat` to continue the conversation.
    pub session_id: String,
    /// Id of the response object inside the instance.
    pub response_id: String,
    /// `completed` for a successful turn.
    pub status: String,
    /// Token usage when the driver reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
}

/// The MCP server over a [`Backend`].
pub struct WarrenServer<B: Backend> {
    backend: B,
    create_timeout: Duration,
    poll_interval: Duration,
    tool_router: ToolRouter<Self>,
}

impl<B: Backend> WarrenServer<B> {
    /// A server with the default 120 s create wait and 2 s poll interval.
    pub fn new(backend: B) -> Self {
        WarrenServer {
            backend,
            create_timeout: DEFAULT_CREATE_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            tool_router: Self::tool_router(),
        }
    }

    /// Override how long `create_instance` waits and how often it polls.
    pub fn with_timing(mut self, create_timeout: Duration, poll_interval: Duration) -> Self {
        self.create_timeout = create_timeout;
        self.poll_interval = poll_interval;
        self
    }

    /// The backend in use.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The static resource list: just `warren://instances`.
    pub fn resources() -> Vec<Resource> {
        vec![
            Resource::new(INSTANCES_RESOURCE_URI, "instances")
                .with_title("Warren instances")
                .with_description("Every instance in the workspace as JSON: {\"data\": [instance...]}, newest first.")
                .with_mime_type("application/json"),
        ]
    }

    /// Body of the `warren://instances` resource.
    pub async fn instances_resource(&self) -> std::result::Result<ReadResourceResult, ErrorData> {
        let instances = self
            .backend
            .list_instances()
            .await
            .map_err(protocol_error)?;
        let text = serde_json::to_string_pretty(&List::new(instances))
            .map_err(|e| protocol_error(Error::from(e)))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, INSTANCES_RESOURCE_URI).with_mime_type("application/json"),
        ]))
    }

    /// Poll until the instance runs, fails or the create timeout passes.
    /// Returns the latest instance and whether it is running.
    async fn wait_until_running(&self, mut inst: Instance) -> WarrenResult<(Instance, bool)> {
        let started = tokio::time::Instant::now();
        loop {
            match inst.status {
                InstanceStatus::Running => return Ok((inst, true)),
                InstanceStatus::Failed | InstanceStatus::Deleting | InstanceStatus::Deleted => {
                    let reason = if inst.status_reason.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", inst.status_reason)
                    };
                    let message = format!(
                        "instance {} reached status {}{reason} instead of running; inspect it with get_instance or remove it with delete_instance",
                        inst.id, inst.status
                    );
                    return Err(Error::new(CODE_INSTANCE_FAILED, 500, message)
                        .with_details(json!({ "instance": inst })));
                }
                _ => {}
            }
            if started.elapsed() >= self.create_timeout {
                return Ok((inst, false));
            }
            tokio::time::sleep(self.poll_interval).await;
            inst = self.backend.get_instance(&inst.id).await?;
        }
    }
}

/// Wire name of an Agent API response status.
fn response_status(status: ResponseStatus) -> &'static str {
    match status {
        ResponseStatus::InProgress => "in_progress",
        ResponseStatus::Completed => "completed",
        ResponseStatus::Failed => "failed",
        ResponseStatus::Cancelled => "cancelled",
    }
}

/// A backend failure while serving a resource: a JSON-RPC error carrying the Warren error.
fn protocol_error(e: Error) -> ErrorData {
    ErrorData::internal_error(
        format!("{}: {}", e.code, e.message),
        Some(json!({ "error": e })),
    )
}

#[tool_router]
impl<B: Backend> WarrenServer<B> {
    #[tool(
        description = "List every instance in the workspace, newest first, as {\"data\": [instance...]}. Each instance has id, name, template, type, status (provisioning|running|stopping|stopped|starting|restarting|updating|sleeping|waking|failed|deleting|deleted), status_reason, resources {cpu, memory, disk}, url, auto_sleep, idle_timeout_seconds and timestamps.",
        annotations(
            title = "List instances",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_instances(&self) -> ToolOutcome {
        let instances = self.backend.list_instances().await?;
        tracing::debug!(count = instances.len(), "list_instances");
        Ok(ok_json(&List::new(instances))?)
    }

    #[tool(
        description = "Get one instance by id: status, resources, template, URL, timestamps and disk usage. Use it to poll a provisioning or waking instance.",
        annotations(title = "Get instance", read_only_hint = true, idempotent_hint = true)
    )]
    async fn get_instance(&self, Parameters(args): Parameters<InstanceIdArgs>) -> ToolOutcome {
        let id = args.id()?;
        let inst = self.backend.get_instance(id).await?;
        Ok(ok_json(&inst)?)
    }

    #[tool(
        description = "Create a persistent instance (Linux, or macOS with type mac) and wait up to 120 seconds for it to be running, then return the instance object. All arguments are optional: template (default warren-base; warren-agent for chat), type default|vm|mac, cpu/memory/disk, name, auto_sleep, idle_timeout_seconds, env. The workspace's awake-instance limit applies (instance_limit_reached); a mac host that already runs two macOS VMs answers host_capacity_reached. If the instance is not running after the wait it is still returned with its current status; poll get_instance.",
        annotations(
            title = "Create instance",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_instance(
        &self,
        Parameters(args): Parameters<CreateInstanceArgs>,
    ) -> ToolOutcome {
        let req = args.into_request()?;
        let created = self.backend.create_instance(req).await?;
        tracing::info!(id = %created.id, template = %created.template, "created instance");
        let (inst, running) = self.wait_until_running(created).await?;
        let mut result = ok_json(&inst)?;
        if !running {
            tracing::warn!(id = %inst.id, status = %inst.status, "instance not running after the create wait");
            result.content.insert(
                0,
                ContentBlock::text(format!(
                    "Instance {} was created but is still {} after {} seconds; poll get_instance until it is running.",
                    inst.id,
                    inst.status,
                    self.create_timeout.as_secs()
                )),
            );
        }
        Ok(result)
    }

    #[tool(
        description = "Permanently delete an instance and its disk. Returns {id, status: \"deleting\"}; the final minutes are settled. This cannot be undone.",
        annotations(
            title = "Delete instance",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn delete_instance(&self, Parameters(args): Parameters<InstanceIdArgs>) -> ToolOutcome {
        let id = args.id()?;
        let ack = self.backend.delete_instance(id).await?;
        tracing::info!(id, "deleting instance");
        Ok(ok_json(&ack)?)
    }

    #[tool(
        description = "Stop a running or sleeping instance. Files are kept and only disk is billed; a stopped instance never wakes on its own and must be started with start_instance. Returns {id, status: \"stopping\"}.",
        annotations(
            title = "Stop instance",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn stop_instance(&self, Parameters(args): Parameters<InstanceIdArgs>) -> ToolOutcome {
        let id = args.id()?;
        let ack = self.backend.stop_instance(id).await?;
        Ok(ok_json(&ack)?)
    }

    #[tool(
        description = "Start a stopped or sleeping instance (fresh boot, files intact). Enforces the workspace awake limit. Returns {id, status: \"starting\"}; poll get_instance until running.",
        annotations(
            title = "Start instance",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn start_instance(&self, Parameters(args): Parameters<InstanceIdArgs>) -> ToolOutcome {
        let id = args.id()?;
        let ack = self.backend.start_instance(id).await?;
        Ok(ok_json(&ack)?)
    }

    #[tool(
        description = "Put an instance to sleep now: processes stop, files persist, only disk is billed, and the next request or exec wakes it automatically. Returns {id, status: \"sleeping\"}.",
        annotations(
            title = "Sleep instance",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn sleep_instance(&self, Parameters(args): Parameters<InstanceIdArgs>) -> ToolOutcome {
        let id = args.id()?;
        let ack = self.backend.sleep_instance(id).await?;
        Ok(ok_json(&ack)?)
    }

    #[tool(
        description = "Run a shell command inside an instance and wait for it to finish. Returns {exit_code, stdout, stderr, duration_ms, truncated}; stdout and stderr are capped at 1 MiB each. A non-zero exit_code is reported, not treated as a tool error. Wakes a sleeping instance; fails with conflict when it is stopped. Optional user (default: the image user, \"root\" allowed) and timeout_seconds (default 900).",
        annotations(
            title = "Run command",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn exec(&self, Parameters(args): Parameters<ExecArgs>) -> ToolOutcome {
        let (id, req) = args.into_request()?;
        tracing::debug!(id = %id, command = %req.command, "exec");
        let result = self.backend.exec(&id, req).await?;
        Ok(ok_json(&result)?)
    }

    #[tool(
        description = "Read a file from the instance's persistent home directory. Returns {path, size, truncated, encoding, content}: encoding is \"utf-8\" with the text in content, or \"base64\" for binary files. Files above 1 MiB are cut at 1 MiB with truncated: true. Paths are relative to the home; \"..\" or paths outside it are forbidden.",
        annotations(title = "Read file", read_only_hint = true, idempotent_hint = true)
    )]
    async fn read_file(&self, Parameters(args): Parameters<ReadFileArgs>) -> ToolOutcome {
        let id = crate::args::instance_id(&args.id)?;
        let path = file_path(&args.path)?;
        let bytes = self.backend.read_file(id, path).await?;
        Ok(ok_json(&FileContent::from_bytes(path, bytes))?)
    }

    #[tool(
        description = "Create or replace a file in the instance's persistent home directory with the given UTF-8 text. Parent directories are created. Returns {path, size}.",
        annotations(
            title = "Write file",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn write_file(&self, Parameters(args): Parameters<WriteFileArgs>) -> ToolOutcome {
        let id = crate::args::instance_id(&args.id)?;
        let path = file_path(&args.path)?;
        let written = self
            .backend
            .write_file(id, path, args.content.into_bytes())
            .await?;
        Ok(ok_json(&written)?)
    }

    #[tool(
        description = "List a directory inside the instance's persistent home. Returns {path, entries: [{name, type: \"file\"|\"dir\", size, modified_at}]} (modified_at in unix milliseconds). path defaults to \".\" (the home).",
        annotations(title = "List files", read_only_hint = true, idempotent_hint = true)
    )]
    async fn list_files(&self, Parameters(args): Parameters<ListFilesArgs>) -> ToolOutcome {
        let id = crate::args::instance_id(&args.id)?;
        let path = args.path()?;
        let listing = self.backend.list_files(id, path).await?;
        Ok(ok_json(&listing)?)
    }

    #[tool(
        description = "Send a message to the agent running inside an agent-template instance (e.g. warren-agent, warren-opencode) and wait for its full reply. Returns {output_text, session_id, response_id, status}. Pass the returned session_id to continue the same conversation. Fails with not_supported on templates without an agent.",
        annotations(
            title = "Chat with the instance agent",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn chat(&self, Parameters(args): Parameters<ChatArgs>) -> ToolOutcome {
        let (id, req) = args.into_request()?;
        let resp = self.backend.chat(&id, req).await?;
        if resp.status == ResponseStatus::Failed {
            let message = resp
                .error
                .clone()
                .unwrap_or_else(|| "the agent reported a failed turn".to_string());
            return Err(Error::new(CODE_RESPONSE_FAILED, 502, message)
                .with_details(json!({ "response": resp }))
                .into());
        }
        let reply = ChatReply {
            output_text: resp.output_text,
            session_id: resp.session_id,
            response_id: resp.id,
            status: response_status(resp.status).to_string(),
            usage: resp.usage,
        };
        Ok(ok_json(&reply)?)
    }

    #[tool(
        description = "Create a time-limited URL that opens a port of the instance in a browser without an API key (e.g. port 7681 for the web terminal). Returns {url, expires_at}. port defaults to the template's default port; ttl is 60..86400 seconds, default 3600.",
        annotations(title = "Signed URL", read_only_hint = true, idempotent_hint = false)
    )]
    async fn signed_url(&self, Parameters(args): Parameters<SignedUrlArgs>) -> ToolOutcome {
        let (id, port, ttl) = args.parts()?;
        let signed = self.backend.signed_url(id, port, ttl).await?;
        Ok(ok_json(&signed)?)
    }

    #[tool(
        description = "List the templates an instance can be created from: the system catalog (warren-base, warren-agent, warren-opencode) followed by workspace templates. Each has name, description, agent (true when chat works), default_shape, types and revision.",
        annotations(
            title = "List templates",
            read_only_hint = true,
            idempotent_hint = true
        )
    )]
    async fn list_templates(&self) -> ToolOutcome {
        let templates = self.backend.list_templates().await?;
        Ok(ok_json(&List::new(templates))?)
    }
}

#[tool_handler(router = self.tool_router)]
impl<B: Backend> ServerHandler for WarrenServer<B> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("warren", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(Self::resources()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        if request.uri == INSTANCES_RESOURCE_URI {
            return Ok(self.instances_resource().await?.into());
        }
        Err(ErrorData::resource_not_found(
            format!(
                "unknown resource {}; this server offers {INSTANCES_RESOURCE_URI}",
                request.uri
            ),
            Some(json!({ "uri": request.uri })),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use warren_core::{
        CreateInstanceRequest, ExecRequest, ExecResult, Shape, Template,
        agentapi::{DirListing, FileEntry, ResponseObject, ResponsesRequest},
    };

    use super::*;
    use crate::backend::{LifecycleAck, SignedUrl, WrittenFile};

    const ID: &str = "ab12cd34ef";

    #[derive(Default)]
    struct State {
        instances: Vec<Instance>,
        templates: Vec<Template>,
        files: BTreeMap<String, Vec<u8>>,
        creates: Vec<CreateInstanceRequest>,
        execs: Vec<(String, ExecRequest)>,
        chats: Vec<(String, ResponsesRequest)>,
        calls: Vec<String>,
        /// After this many `get_instance` polls every instance takes this status.
        settle: Option<(u32, InstanceStatus)>,
        polls: u32,
        /// When set, every call fails with this error.
        fail: Option<Error>,
        reply: Option<ResponseObject>,
    }

    #[derive(Default)]
    struct Fake(Mutex<State>);

    impl Fake {
        fn with(f: impl FnOnce(&mut State)) -> Self {
            let fake = Fake::default();
            f(&mut fake.0.lock().unwrap());
            fake
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        fn enter(&self, name: &str) -> WarrenResult<std::sync::MutexGuard<'_, State>> {
            let mut st = self.state();
            st.calls.push(name.to_string());
            match &st.fail {
                Some(e) => Err(e.clone()),
                None => Ok(st),
            }
        }
    }

    fn instance(id: &str, status: InstanceStatus) -> Instance {
        Instance {
            id: id.into(),
            workspace_id: "ws_test".into(),
            template: "warren-base".into(),
            template_name: "warren-base".into(),
            status,
            shape: Shape {
                cpu: 2,
                memory: 4,
                disk: 4,
            },
            url: format!("http://{id}.warren.localhost:7337"),
            default_port: 3737,
            ..Default::default()
        }
    }

    fn ack(id: &str, status: InstanceStatus) -> LifecycleAck {
        LifecycleAck {
            id: id.into(),
            status,
        }
    }

    impl Backend for Fake {
        async fn list_instances(&self) -> WarrenResult<Vec<Instance>> {
            Ok(self.enter("list_instances")?.instances.clone())
        }
        async fn get_instance(&self, id: &str) -> WarrenResult<Instance> {
            let mut st = self.enter("get_instance")?;
            st.polls += 1;
            if let Some((after, status)) = st.settle
                && st.polls >= after
            {
                for i in st.instances.iter_mut() {
                    i.status = status;
                    if status == InstanceStatus::Failed {
                        i.status_reason = "image pull failed".into();
                    }
                }
            }
            st.instances
                .iter()
                .find(|i| i.id == id)
                .cloned()
                .ok_or_else(|| Error::not_found("instance"))
        }
        async fn create_instance(&self, req: CreateInstanceRequest) -> WarrenResult<Instance> {
            let mut st = self.enter("create_instance")?;
            let mut inst = instance(ID, InstanceStatus::Provisioning);
            inst.template = req.template.clone().unwrap_or_else(|| "warren-base".into());
            st.creates.push(req);
            st.instances.push(inst.clone());
            Ok(inst)
        }
        async fn delete_instance(&self, id: &str) -> WarrenResult<LifecycleAck> {
            drop(self.enter("delete_instance")?);
            Ok(ack(id, InstanceStatus::Deleting))
        }
        async fn stop_instance(&self, id: &str) -> WarrenResult<LifecycleAck> {
            drop(self.enter("stop_instance")?);
            Ok(ack(id, InstanceStatus::Stopping))
        }
        async fn start_instance(&self, id: &str) -> WarrenResult<LifecycleAck> {
            drop(self.enter("start_instance")?);
            Ok(ack(id, InstanceStatus::Starting))
        }
        async fn sleep_instance(&self, id: &str) -> WarrenResult<LifecycleAck> {
            drop(self.enter("sleep_instance")?);
            Ok(ack(id, InstanceStatus::Sleeping))
        }
        async fn exec(&self, id: &str, req: ExecRequest) -> WarrenResult<ExecResult> {
            let mut st = self.enter("exec")?;
            let out = format!("ran: {}", req.command);
            st.execs.push((id.to_string(), req));
            Ok(ExecResult {
                exit_code: 3,
                stdout: out,
                stderr: "warn".into(),
                duration_ms: 12,
                truncated: false,
            })
        }
        async fn read_file(&self, _id: &str, path: &str) -> WarrenResult<Vec<u8>> {
            let st = self.enter("read_file")?;
            st.files
                .get(path)
                .cloned()
                .ok_or_else(|| Error::not_found("file"))
        }
        async fn write_file(
            &self,
            _id: &str,
            path: &str,
            content: Vec<u8>,
        ) -> WarrenResult<WrittenFile> {
            let mut st = self.enter("write_file")?;
            let size = content.len() as u64;
            st.files.insert(path.to_string(), content);
            Ok(WrittenFile {
                path: path.to_string(),
                size,
            })
        }
        async fn list_files(&self, _id: &str, path: &str) -> WarrenResult<DirListing> {
            let st = self.enter("list_files")?;
            let entries = st
                .files
                .keys()
                .map(|name| FileEntry {
                    name: name.clone(),
                    kind: "file".into(),
                    size: 1,
                    modified_at: 1_763_000_000_000,
                })
                .collect();
            Ok(DirListing {
                path: path.to_string(),
                entries,
            })
        }
        async fn chat(&self, id: &str, req: ResponsesRequest) -> WarrenResult<ResponseObject> {
            let mut st = self.enter("chat")?;
            st.chats.push((id.to_string(), req.clone()));
            Ok(st.reply.clone().unwrap_or(ResponseObject {
                id: "resp_1".into(),
                session_id: req.session_id.unwrap_or_else(|| "sess_new".into()),
                status: ResponseStatus::Completed,
                output_text: format!("echo: {}", req.input),
                created_at: 1,
                completed_at: Some(2),
                usage: None,
                error: None,
            }))
        }
        async fn signed_url(
            &self,
            id: &str,
            port: Option<u16>,
            ttl: Option<u32>,
        ) -> WarrenResult<SignedUrl> {
            drop(self.enter("signed_url")?);
            let port = port.unwrap_or(3737);
            Ok(SignedUrl {
                url: format!("http://{id}-{port}.warren.localhost:7337/?warren_token=t"),
                expires_at: 1_763_000_000 + i64::from(ttl.unwrap_or(3600)),
            })
        }
        async fn list_templates(&self) -> WarrenResult<Vec<Template>> {
            Ok(self.enter("list_templates")?.templates.clone())
        }
    }

    fn server(fake: Fake) -> WarrenServer<Fake> {
        WarrenServer::new(fake).with_timing(Duration::from_secs(5), Duration::from_millis(1))
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn structured(result: &CallToolResult) -> serde_json::Value {
        result
            .structured_content
            .clone()
            .expect("structured content")
    }

    const TOOLS: [&str; 14] = [
        "chat",
        "create_instance",
        "delete_instance",
        "exec",
        "get_instance",
        "list_files",
        "list_instances",
        "list_templates",
        "read_file",
        "signed_url",
        "sleep_instance",
        "start_instance",
        "stop_instance",
        "write_file",
    ];

    #[test]
    fn tool_schemas_are_objects_with_descriptions() {
        let tools = WarrenServer::<Fake>::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, TOOLS);
        for tool in &tools {
            assert_eq!(
                tool.input_schema.get("type"),
                Some(&json!("object")),
                "{}",
                tool.name
            );
            assert!(
                tool.description.as_deref().is_some_and(|d| d.len() > 40),
                "{} needs a description",
                tool.name
            );
            assert!(
                tool.annotations.is_some(),
                "{} needs annotations",
                tool.name
            );
            assert!(
                tool.input_schema.get("title").is_none(),
                "{} leaks the Rust type name",
                tool.name
            );
        }
        let by_name = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .unwrap()
                .input_schema
                .clone()
        };

        let create = by_name("create_instance");
        let props = create["properties"].as_object().unwrap();
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "auto_sleep",
                "cpu",
                "disk",
                "env",
                "idle_timeout_seconds",
                "memory",
                "name",
                "template",
                "type"
            ]
        );
        assert!(
            create.get("required").is_none(),
            "every create argument is optional"
        );
        assert_eq!(
            props["env"]["additionalProperties"],
            json!({ "type": "string" })
        );
        assert!(
            props["template"]["description"]
                .as_str()
                .unwrap()
                .contains("warren-base")
        );

        let exec = by_name("exec");
        assert_eq!(exec["required"], json!(["id", "command"]));
        assert_eq!(exec["properties"]["command"]["type"], json!("string"));

        let read = by_name("read_file");
        assert_eq!(read["required"], json!(["id", "path"]));
        let write = by_name("write_file");
        assert_eq!(write["required"], json!(["id", "path", "content"]));
        let list = by_name("list_files");
        assert_eq!(list["required"], json!(["id"]));
        let chat = by_name("chat");
        assert_eq!(chat["required"], json!(["id", "message"]));
        let signed = by_name("signed_url");
        assert_eq!(signed["required"], json!(["id"]));
        assert!(
            signed["properties"]["port"]["type"]
                .as_array()
                .unwrap()
                .contains(&json!("integer"))
        );
        for name in ["list_instances", "list_templates"] {
            assert_eq!(
                by_name(name)["properties"],
                json!({}),
                "{name} takes no arguments"
            );
        }
        let get = by_name("get_instance");
        assert_eq!(get["required"], json!(["id"]));
        assert!(
            get["properties"]["id"]["description"]
                .as_str()
                .unwrap()
                .contains("ab12cd34ef")
        );
    }

    #[test]
    fn server_info_names_warren_with_tools_and_resources() {
        let info = server(Fake::default()).get_info();
        assert_eq!(info.server_info.name, "warren");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
        assert!(
            info.instructions
                .as_deref()
                .is_some_and(|i| i.contains("create_instance"))
        );
        let resources = WarrenServer::<Fake>::resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, INSTANCES_RESOURCE_URI);
        assert_eq!(resources[0].mime_type.as_deref(), Some("application/json"));
    }

    #[tokio::test]
    async fn list_and_get_return_json() {
        let srv = server(Fake::with(|st| {
            st.instances.push(instance(ID, InstanceStatus::Running));
            st.instances
                .push(instance("zz99zz99zz", InstanceStatus::Sleeping));
        }));
        let result = srv.list_instances().await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let data = structured(&result)["data"].as_array().unwrap().clone();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], json!(ID));
        assert_eq!(data[0]["status"], json!("running"));
        assert!(text(&result).contains("\"resources\""));

        let result = srv
            .get_instance(Parameters(InstanceIdArgs {
                id: format!(" {ID} "),
            }))
            .await
            .unwrap();
        assert_eq!(structured(&result)["id"], json!(ID));
        assert_eq!(
            structured(&result)["url"],
            json!(format!("http://{ID}.warren.localhost:7337"))
        );
    }

    #[tokio::test]
    async fn create_waits_until_running() {
        let srv = server(Fake::with(|st| {
            st.settle = Some((3, InstanceStatus::Running))
        }));
        let args: CreateInstanceArgs = serde_json::from_value(
            json!({ "template": "warren-agent", "cpu": 4, "memory": 8, "env": { "A_B": "1" } }),
        )
        .unwrap();
        let result = srv.create_instance(Parameters(args)).await.unwrap();
        assert_eq!(result.is_error, Some(false));
        let inst = structured(&result);
        assert_eq!(inst["status"], json!("running"));
        assert_eq!(inst["template"], json!("warren-agent"));
        assert_eq!(result.content.len(), 1, "no warning when running");
        let st = srv.backend().state();
        assert_eq!(st.polls, 3);
        assert_eq!(st.creates.len(), 1);
        assert_eq!(st.creates[0].shape.unwrap().cpu, Some(4));
        assert_eq!(st.creates[0].env.as_ref().unwrap()["A_B"], "1");
    }

    #[tokio::test]
    async fn create_returns_the_instance_with_a_warning_on_timeout() {
        let srv = WarrenServer::new(Fake::default())
            .with_timing(Duration::from_millis(20), Duration::from_millis(1));
        let result = srv
            .create_instance(Parameters(CreateInstanceArgs::default()))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(structured(&result)["status"], json!("provisioning"));
        assert_eq!(result.content.len(), 2);
        assert!(
            text(&result).contains("still provisioning"),
            "{}",
            text(&result)
        );
        assert!(srv.backend().state().polls >= 1);
    }

    #[tokio::test]
    async fn create_reports_failed_instances_as_tool_errors() {
        let srv = server(Fake::with(|st| {
            st.settle = Some((1, InstanceStatus::Failed))
        }));
        let outcome = srv
            .create_instance(Parameters(CreateInstanceArgs::default()))
            .await;
        let err = outcome.unwrap_err();
        assert_eq!(err.0.code, CODE_INSTANCE_FAILED);
        assert!(err.0.message.contains("image pull failed"));
        let result = match err.into_call_tool_result().unwrap() {
            CallToolResponse::Complete(r) => r,
            other => panic!("unexpected response {other:?}"),
        };
        assert_eq!(result.is_error, Some(true));
        assert!(text(&result).starts_with("instance_failed: "));
        assert_eq!(
            structured(&result)["error"]["details"]["instance"]["id"],
            json!(ID)
        );
    }

    #[tokio::test]
    async fn create_rejects_bad_arguments_before_calling_the_api() {
        let srv = server(Fake::default());
        let args: CreateInstanceArgs =
            serde_json::from_value(json!({ "type": "mainframe" })).unwrap();
        let err = srv.create_instance(Parameters(args)).await.unwrap_err();
        assert_eq!(err.0.code, warren_core::errors::CODE_INVALID_REQUEST);
        assert!(srv.backend().state().calls.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_tools_return_acks() {
        let srv = server(Fake::default());
        let args = || Parameters(InstanceIdArgs { id: ID.into() });
        let stop = srv.stop_instance(args()).await.unwrap();
        assert_eq!(structured(&stop), json!({ "id": ID, "status": "stopping" }));
        let start = srv.start_instance(args()).await.unwrap();
        assert_eq!(structured(&start)["status"], json!("starting"));
        let sleep = srv.sleep_instance(args()).await.unwrap();
        assert_eq!(structured(&sleep)["status"], json!("sleeping"));
        let delete = srv.delete_instance(args()).await.unwrap();
        assert_eq!(structured(&delete)["status"], json!("deleting"));
        assert_eq!(
            srv.backend().state().calls,
            [
                "stop_instance",
                "start_instance",
                "sleep_instance",
                "delete_instance"
            ]
        );
    }

    #[tokio::test]
    async fn exec_passes_the_request_through_and_reports_exit_codes() {
        let srv = server(Fake::default());
        let args: ExecArgs = serde_json::from_value(
            json!({ "id": ID, "command": "make test", "user": "root", "timeout_seconds": 30 }),
        )
        .unwrap();
        let result = srv.exec(Parameters(args)).await.unwrap();
        assert_eq!(
            result.is_error,
            Some(false),
            "a non-zero exit code is data, not a tool error"
        );
        let out = structured(&result);
        assert_eq!(out["exit_code"], json!(3));
        assert_eq!(out["stdout"], json!("ran: make test"));
        assert_eq!(out["stderr"], json!("warn"));
        let st = srv.backend().state();
        assert_eq!(st.execs[0].0, ID);
        assert_eq!(
            st.execs[0].1,
            ExecRequest {
                command: "make test".into(),
                user: "root".into(),
                timeout_seconds: 30,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn files_round_trip_text_and_binary() {
        let srv = server(Fake::with(|st| {
            st.files.insert("bin.dat".into(), vec![0xFF, 0x00]);
        }));
        let written = srv
            .write_file(Parameters(WriteFileArgs {
                id: ID.into(),
                path: "notes.md".into(),
                content: "héllo".into(),
            }))
            .await
            .unwrap();
        assert_eq!(
            structured(&written),
            json!({ "path": "notes.md", "size": 6 })
        );

        let read = srv
            .read_file(Parameters(ReadFileArgs {
                id: ID.into(),
                path: "notes.md".into(),
            }))
            .await
            .unwrap();
        assert_eq!(
            structured(&read),
            json!({ "path": "notes.md", "size": 6, "truncated": false, "encoding": "utf-8", "content": "héllo" })
        );

        let read = srv
            .read_file(Parameters(ReadFileArgs {
                id: ID.into(),
                path: "bin.dat".into(),
            }))
            .await
            .unwrap();
        let body = structured(&read);
        assert_eq!(body["encoding"], json!("base64"));
        assert_eq!(body["content"], json!("/wA="));

        let listing = srv
            .list_files(Parameters(ListFilesArgs {
                id: ID.into(),
                path: None,
            }))
            .await
            .unwrap();
        let body = structured(&listing);
        assert_eq!(body["path"], json!("."));
        assert_eq!(body["entries"].as_array().unwrap().len(), 2);
        assert_eq!(body["entries"][0]["type"], json!("file"));

        let missing = srv
            .read_file(Parameters(ReadFileArgs {
                id: ID.into(),
                path: "nope".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(missing.0.code, warren_core::errors::CODE_NOT_FOUND);
        let empty = srv
            .read_file(Parameters(ReadFileArgs {
                id: ID.into(),
                path: "  ".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(empty.0.code, warren_core::errors::CODE_INVALID_REQUEST);
    }

    #[test]
    fn file_content_caps_and_decodes() {
        let small = FileContent::from_bytes("a", b"plain".to_vec());
        assert_eq!(
            (
                small.encoding,
                small.content.as_str(),
                small.truncated,
                small.size
            ),
            ("utf-8", "plain", false, 5)
        );

        let mut big = vec![b'x'; MAX_READ_BYTES - 1];
        big.extend_from_slice("é".as_bytes()); // the cap falls inside this character
        big.extend_from_slice(b"tail");
        let capped = FileContent::from_bytes("b", big.clone());
        assert!(capped.truncated);
        assert_eq!(capped.size, big.len());
        assert_eq!(capped.encoding, "utf-8");
        assert_eq!(capped.content.len(), MAX_READ_BYTES - 1);

        let binary = FileContent::from_bytes("c", vec![0x00, 0xFF, 0xFE]);
        assert_eq!(binary.encoding, "base64");
        assert_eq!(binary.content, base64_encode(&[0x00, 0xFF, 0xFE]));
    }

    #[test]
    fn base64_matches_rfc_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected);
        }
        assert_eq!(base64_encode(&[0xFF, 0x00]), "/wA=");
    }

    #[tokio::test]
    async fn chat_returns_output_and_session() {
        let srv = server(Fake::default());
        let args: ChatArgs =
            serde_json::from_value(json!({ "id": ID, "message": "hello" })).unwrap();
        let result = srv.chat(Parameters(args)).await.unwrap();
        let body = structured(&result);
        assert_eq!(body["output_text"], json!("echo: hello"));
        assert_eq!(body["session_id"], json!("sess_new"));
        assert_eq!(body["status"], json!("completed"));
        assert_eq!(body["response_id"], json!("resp_1"));
        assert!(body.get("usage").is_none());

        let args: ChatArgs = serde_json::from_value(
            json!({ "id": ID, "message": "again", "session_id": "sess_new" }),
        )
        .unwrap();
        let result = srv.chat(Parameters(args)).await.unwrap();
        assert_eq!(structured(&result)["session_id"], json!("sess_new"));
        let st = srv.backend().state();
        assert_eq!(st.chats.len(), 2);
        assert_eq!(st.chats[1].1.session_id.as_deref(), Some("sess_new"));
        assert_eq!(st.chats[1].1.stream, Some(false));
    }

    #[tokio::test]
    async fn chat_failures_are_tool_errors() {
        let srv = server(Fake::with(|st| {
            st.reply = Some(ResponseObject {
                id: "resp_2".into(),
                session_id: "s".into(),
                status: ResponseStatus::Failed,
                output_text: String::new(),
                created_at: 1,
                completed_at: None,
                usage: None,
                error: Some("model unavailable".into()),
            });
        }));
        let args: ChatArgs =
            serde_json::from_value(json!({ "id": ID, "message": "hello" })).unwrap();
        let err = srv.chat(Parameters(args)).await.unwrap_err();
        assert_eq!(err.0.code, CODE_RESPONSE_FAILED);
        assert_eq!(err.0.message, "model unavailable");
    }

    #[tokio::test]
    async fn signed_url_and_templates() {
        let srv = server(Fake::with(|st| {
            st.templates.push(Template {
                name: "warren-base".into(),
                agent: false,
                ..Default::default()
            });
        }));
        let args: SignedUrlArgs =
            serde_json::from_value(json!({ "id": ID, "port": 7681, "ttl": 600 })).unwrap();
        let result = srv.signed_url(Parameters(args)).await.unwrap();
        let body = structured(&result);
        assert_eq!(
            body["url"],
            json!(format!(
                "http://{ID}-7681.warren.localhost:7337/?warren_token=t"
            ))
        );
        assert_eq!(body["expires_at"], json!(1_763_000_600));

        let result = srv.list_templates().await.unwrap();
        assert_eq!(structured(&result)["data"][0]["name"], json!("warren-base"));
    }

    #[tokio::test]
    async fn api_errors_become_tool_errors_with_code_and_message() {
        let srv = server(Fake::with(|st| st.fail = Some(Error::instance_limit(3, 3))));
        let err = srv
            .create_instance(Parameters(CreateInstanceArgs::default()))
            .await
            .unwrap_err();
        let result = match err.into_call_tool_result().unwrap() {
            CallToolResponse::Complete(r) => r,
            other => panic!("unexpected response {other:?}"),
        };
        assert_eq!(result.is_error, Some(true));
        let shown = text(&result);
        assert!(
            shown.starts_with("instance_limit_reached: This workspace may keep 3 instances"),
            "{shown}"
        );
        let body = structured(&result);
        assert_eq!(body["error"]["code"], json!("instance_limit_reached"));
        assert_eq!(body["error"]["details"]["awake"], json!(3));
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected_without_a_round_trip() {
        let srv = server(Fake::default());
        let err = srv
            .get_instance(Parameters(InstanceIdArgs { id: "NOPE".into() }))
            .await
            .unwrap_err();
        assert_eq!(err.0.code, warren_core::errors::CODE_INVALID_REQUEST);
        assert!(err.0.message.contains("NOPE"));
        assert!(srv.backend().state().calls.is_empty());
    }

    #[tokio::test]
    async fn instances_resource_lists_instances_as_json() {
        let srv = server(Fake::with(|st| {
            st.instances.push(instance(ID, InstanceStatus::Running))
        }));
        let result = srv.instances_resource().await.unwrap();
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => {
                assert_eq!(uri, INSTANCES_RESOURCE_URI);
                assert_eq!(mime_type.as_deref(), Some("application/json"));
                let body: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(body["data"][0]["id"], json!(ID));
            }
            other => panic!("unexpected contents {other:?}"),
        }

        let failing = server(Fake::with(|st| st.fail = Some(Error::invalid_api_key(""))));
        let err = failing.instances_resource().await.unwrap_err();
        assert!(err.message.starts_with("invalid_api_key: "));
        assert_eq!(err.data.unwrap()["error"]["code"], json!("invalid_api_key"));
    }
}
