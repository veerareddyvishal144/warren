//! Tool argument types. Their derived JSON Schemas (field docs included) are
//! exactly what MCP clients show the model, so keep the docs precise.

use std::collections::BTreeMap;

use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use warren_core::{
    CreateInstanceRequest, Error, ExecRequest, InstanceType, Result, ShapeRequest,
    agentapi::ResponsesRequest, ids,
};

/// Lowest signed-URL lifetime the API accepts, in seconds.
pub const MIN_SIGNED_URL_TTL: u32 = 60;
/// Highest signed-URL lifetime the API accepts, in seconds.
pub const MAX_SIGNED_URL_TTL: u32 = 86_400;

/// Trim and validate an instance id argument.
pub fn instance_id(raw: &str) -> Result<&str> {
    let id = raw.trim();
    if ids::valid_instance_id(id) {
        Ok(id)
    } else {
        Err(Error::invalid(format!(
            "{raw:?} is not an instance id: expected 10 lowercase letters or digits starting with a letter, like \"ab12cd34ef\""
        )))
    }
}

/// A non-empty path relative to the instance home.
pub fn file_path(raw: &str) -> Result<&str> {
    let path = raw.trim();
    if path.is_empty() {
        Err(Error::invalid(
            "path must not be empty; use \".\" for the home directory",
        ))
    } else {
        Ok(path)
    }
}

fn non_empty<'a>(raw: &'a str, what: &str) -> Result<&'a str> {
    if raw.trim().is_empty() {
        Err(Error::invalid(format!("{what} must not be empty")))
    } else {
        Ok(raw)
    }
}

/// Tools that act on one instance.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct InstanceIdArgs {
    /// Instance id: 10 lowercase letters or digits starting with a letter, e.g. "ab12cd34ef".
    pub id: String,
}

impl InstanceIdArgs {
    /// The validated id.
    pub fn id(&self) -> Result<&str> {
        instance_id(&self.id)
    }
}

/// `create_instance`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateInstanceArgs {
    /// Template name, optionally pinned as "name@revision". Default "warren-base" (plain Linux: Node, Python, git, browser terminal, files API, no agent). "warren-agent" adds the built-in chat agent so `chat` works; "warren-opencode" runs OpenCode. Use `list_templates` for the full catalog.
    pub template: Option<String>,
    /// Isolation type: "default" (gVisor container) or "vm" (Firecracker microVM, compute billed at 2.5x). Default "default".
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// vCPUs. Valid cpu/memory pairs are 2/4, 4/8, 4/16 and 4/32 (GB). Omit for the template default (2 vCPU, 4 GB).
    pub cpu: Option<u32>,
    /// Memory in GB; see `cpu` for the valid pairs.
    pub memory: Option<u32>,
    /// Disk in GB. Whole number; 2..12 GB for the 2/4 shape, up to 20/40/80 GB for the larger shapes and up to 100 GB on type "vm". Omit for the shape default.
    pub disk: Option<u32>,
    /// Human-readable name shown in listings.
    pub name: Option<String>,
    /// Put the instance to sleep after `idle_timeout_seconds` without requests (disk-only billing; any request wakes it). Default false.
    pub auto_sleep: Option<bool>,
    /// Idle timeout for auto_sleep in seconds, 300..86400. Default 900.
    pub idle_timeout_seconds: Option<u32>,
    /// Environment variables injected into the instance. Keys match [A-Z_][A-Z0-9_]*, at most 64 entries, values up to 4 KiB.
    pub env: Option<BTreeMap<String, String>>,
}

impl CreateInstanceArgs {
    /// Translate into the API body, validating what can be checked locally.
    pub fn into_request(self) -> Result<CreateInstanceRequest> {
        let kind = match self
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => Some(InstanceType::parse(s).ok_or_else(|| {
                Error::invalid(format!(
                    "unknown instance type {s:?}; use \"default\" or \"vm\""
                ))
            })?),
        };
        let shape = (self.cpu.is_some() || self.memory.is_some() || self.disk.is_some()).then_some(
            ShapeRequest {
                cpu: self.cpu,
                memory: self.memory,
                disk: self.disk,
            },
        );
        if let Some(env) = &self.env
            && let Some(bad) = env.keys().find(|k| !ids::valid_env_key(k))
        {
            return Err(Error::invalid(format!(
                "invalid env key {bad:?}: keys match [A-Z_][A-Z0-9_]*"
            )));
        }
        let template = self
            .template
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let name = self
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        Ok(CreateInstanceRequest {
            template,
            shape,
            kind,
            name,
            env: self.env,
            auto_sleep: self.auto_sleep,
            idle_timeout_seconds: self.idle_timeout_seconds,
            ..Default::default()
        })
    }
}

/// `exec`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ExecArgs {
    /// Instance id.
    pub id: String,
    /// Shell command line to run inside the instance, e.g. "ls -la ~ && cat notes.txt".
    pub command: String,
    /// Unix user to run as. Omit for the image default ("agent"); "root" is allowed.
    pub user: Option<String>,
    /// Kill the command after this many seconds. Omit for the server default (15 minutes).
    pub timeout_seconds: Option<u32>,
}

impl ExecArgs {
    /// The validated id and request body.
    pub fn into_request(self) -> Result<(String, ExecRequest)> {
        let id = instance_id(&self.id)?.to_string();
        non_empty(&self.command, "command")?;
        let req = ExecRequest {
            command: self.command,
            user: self.user.map(|u| u.trim().to_string()).unwrap_or_default(),
            timeout_seconds: self.timeout_seconds.unwrap_or(0),
            ..Default::default()
        };
        Ok((id, req))
    }
}

/// `read_file`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadFileArgs {
    /// Instance id.
    pub id: String,
    /// File path relative to the instance home directory (e.g. "notes/todo.md"). Paths outside the home are refused.
    pub path: String,
}

/// `write_file`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WriteFileArgs {
    /// Instance id.
    pub id: String,
    /// File path relative to the instance home directory. Missing parent directories are created.
    pub path: String,
    /// Full new contents of the file as UTF-8 text. The file is replaced, not appended to.
    pub content: String,
}

/// `list_files`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListFilesArgs {
    /// Instance id.
    pub id: String,
    /// Directory path relative to the instance home. Default "." (the home directory itself).
    pub path: Option<String>,
}

impl ListFilesArgs {
    /// The directory to list, defaulting to the home.
    pub fn path(&self) -> Result<&str> {
        match self.path.as_deref() {
            None => Ok("."),
            Some(p) => file_path(p),
        }
    }
}

/// `chat`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ChatArgs {
    /// Instance id; the instance must run an agent template such as "warren-agent".
    pub id: String,
    /// The message (prompt) for the agent inside the instance.
    pub message: String,
    /// Continue an earlier conversation: pass the session_id returned by a previous `chat` call. Omit to start a new session.
    pub session_id: Option<String>,
}

impl ChatArgs {
    /// The validated id and request body (always non-streaming).
    pub fn into_request(self) -> Result<(String, ResponsesRequest)> {
        let id = instance_id(&self.id)?.to_string();
        non_empty(&self.message, "message")?;
        let session_id = self
            .session_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok((
            id,
            ResponsesRequest {
                input: self.message,
                session_id,
                stream: Some(false),
                ..Default::default()
            },
        ))
    }
}

/// `signed_url`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SignedUrlArgs {
    /// Instance id.
    pub id: String,
    /// Port inside the instance the URL should reach, e.g. 7681 for the browser terminal or 3737 for the Agent API. Omit for the template's default port.
    pub port: Option<u16>,
    /// Lifetime of the link in seconds, 60..86400. Default 3600.
    pub ttl: Option<u32>,
}

impl SignedUrlArgs {
    /// The validated id, port and ttl.
    pub fn parts(&self) -> Result<(&str, Option<u16>, Option<u32>)> {
        let id = instance_id(&self.id)?;
        let port = self.port.filter(|p| *p != 0);
        if let Some(ttl) = self.ttl
            && !(MIN_SIGNED_URL_TTL..=MAX_SIGNED_URL_TTL).contains(&ttl)
        {
            return Err(Error::invalid(format!(
                "ttl must be between {MIN_SIGNED_URL_TTL} and {MAX_SIGNED_URL_TTL} seconds"
            )));
        }
        Ok((id, port, self.ttl))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn instance_ids_are_validated() {
        assert_eq!(instance_id(" ab12cd34ef ").unwrap(), "ab12cd34ef");
        assert!(instance_id("AB12CD34EF").is_err());
        assert!(instance_id("1b12cd34ef").is_err());
        assert!(instance_id("abc").is_err());
        assert_eq!(
            instance_id("").unwrap_err().code,
            warren_core::errors::CODE_INVALID_REQUEST
        );
    }

    #[test]
    fn create_args_map_to_the_api_body() {
        let args: CreateInstanceArgs = serde_json::from_value(json!({
            "template": "warren-agent", "type": "vm", "cpu": 4, "memory": 8, "name": " bot ",
            "auto_sleep": true, "idle_timeout_seconds": 600, "env": { "OPENAI_API_KEY": "x" }
        }))
        .unwrap();
        let req = args.into_request().unwrap();
        assert_eq!(req.template.as_deref(), Some("warren-agent"));
        assert_eq!(req.kind, Some(InstanceType::Vm));
        assert_eq!(
            req.shape,
            Some(ShapeRequest {
                cpu: Some(4),
                memory: Some(8),
                disk: None
            })
        );
        assert_eq!(req.name.as_deref(), Some("bot"));
        assert_eq!(req.auto_sleep, Some(true));
        assert_eq!(req.idle_timeout_seconds, Some(600));
        assert_eq!(req.env.unwrap()["OPENAI_API_KEY"], "x");
        assert!(req.metadata.is_none() && req.budget.is_none() && req.network.is_none());

        let empty: CreateInstanceArgs = serde_json::from_value(json!({})).unwrap();
        let req = empty.into_request().unwrap();
        assert!(req.template.is_none() && req.shape.is_none() && req.kind.is_none());
        let body = serde_json::to_value(&req).unwrap();
        assert_eq!(body, json!({}), "absent fields must not be sent");
    }

    #[test]
    fn create_args_reject_bad_type_and_env() {
        let args: CreateInstanceArgs = serde_json::from_value(json!({ "type": "huge" })).unwrap();
        assert!(args.into_request().unwrap_err().message.contains("huge"));
        let args: CreateInstanceArgs =
            serde_json::from_value(json!({ "env": { "lower": "x" } })).unwrap();
        assert!(args.into_request().unwrap_err().message.contains("lower"));
    }

    #[test]
    fn exec_and_chat_args() {
        let args: ExecArgs = serde_json::from_value(
            json!({ "id": "ab12cd34ef", "command": "ls", "user": "root", "timeout_seconds": 5 }),
        )
        .unwrap();
        let (id, req) = args.into_request().unwrap();
        assert_eq!(id, "ab12cd34ef");
        assert_eq!(
            req,
            ExecRequest {
                command: "ls".into(),
                user: "root".into(),
                timeout_seconds: 5,
                ..Default::default()
            }
        );
        let args: ExecArgs =
            serde_json::from_value(json!({ "id": "ab12cd34ef", "command": "  " })).unwrap();
        assert!(args.into_request().is_err());

        let args: ChatArgs = serde_json::from_value(
            json!({ "id": "ab12cd34ef", "message": "hi", "session_id": "" }),
        )
        .unwrap();
        let (_, req) = args.into_request().unwrap();
        assert_eq!(req.input, "hi");
        assert_eq!(req.session_id, None);
        assert_eq!(req.stream, Some(false));
    }

    #[test]
    fn signed_url_and_list_files_defaults() {
        let args: SignedUrlArgs =
            serde_json::from_value(json!({ "id": "ab12cd34ef", "port": 0 })).unwrap();
        assert_eq!(args.parts().unwrap(), ("ab12cd34ef", None, None));
        let args: SignedUrlArgs =
            serde_json::from_value(json!({ "id": "ab12cd34ef", "ttl": 10 })).unwrap();
        assert!(args.parts().is_err());
        let args: ListFilesArgs = serde_json::from_value(json!({ "id": "ab12cd34ef" })).unwrap();
        assert_eq!(args.path().unwrap(), ".");
        let args: ListFilesArgs =
            serde_json::from_value(json!({ "id": "ab12cd34ef", "path": " src " })).unwrap();
        assert_eq!(args.path().unwrap(), "src");
    }
}
