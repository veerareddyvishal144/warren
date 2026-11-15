//! The `openai` driver: an agent loop against any OpenAI-compatible
//! chat-completions endpoint with four local tools (`run_shell`, `read_file`,
//! `write_file`, `list_dir`) confined to the home directory.
//!
//! Every call asks for `stream: true`; text deltas are forwarded as they
//! arrive and tool-call fragments are accumulated. Servers that ignore the
//! flag and answer with a plain JSON completion are handled too, in which case
//! the text is streamed in chunks.

use std::{path::Path, time::Duration};

use futures::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use warren_core::{Error, Result};

use super::{Driver, EventSink, Reply, Turn, describe_error, stream_text};
use crate::{exec::run_shell_combined, files::Home, owner::HomeOwner};

/// Tool rounds per turn before the model is asked for a final answer.
pub const MAX_TOOL_ITERATIONS: usize = 12;
/// Model used when neither the request nor `WARREN_MODEL` names one.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
/// Bytes of tool output handed back to the model.
const TOOL_OUTPUT_CAP: usize = 32 * 1024;
/// Bytes returned by `read_file`.
const READ_FILE_CAP: usize = 64 * 1024;
/// `run_shell` commands are killed after this long.
const TOOL_SHELL_TIMEOUT: Duration = Duration::from_secs(300);
/// Whole-request timeout for one model call, body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Characters of an upstream error body kept in our error message.
const ERROR_BODY_CHARS: usize = 600;

/// See the module documentation.
pub struct OpenAiDriver {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    default_model: String,
}

/// One parsed assistant turn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Value>,
}

/// A function call requested by the model.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON text as sent by the model.
    pub arguments: String,
}

impl OpenAiDriver {
    /// `base_url` is the API root (for example `https://api.openai.com/v1`).
    pub fn new(base_url: &str, api_key: &str, default_model: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Error::internal(format!("cannot build HTTP client: {e}")))?;
        let model = if default_model.trim().is_empty() {
            DEFAULT_MODEL
        } else {
            default_model.trim()
        };
        Ok(OpenAiDriver {
            client,
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
            default_model: model.to_string(),
        })
    }

    /// The chat-completions URL.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    async fn complete(
        &self,
        model: &str,
        messages: &[Value],
        allow_tools: bool,
        sink: &dyn EventSink,
    ) -> Result<Completion> {
        let body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "tools": tools_spec(),
            "tool_choice": if allow_tools { "auto" } else { "none" },
        });
        let mut req = self.client.post(self.endpoint()).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.map_err(|e| {
            Error::bad_gateway(format!(
                "cannot reach model endpoint {}: {e}",
                self.endpoint()
            ))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::bad_gateway(format!(
                "model endpoint returned {status}: {}",
                snippet(&text)
            )));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            return read_sse_stream(resp, sink).await;
        }
        let text = resp
            .text()
            .await
            .map_err(|e| Error::bad_gateway(format!("error reading model response: {e}")))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            Error::bad_gateway(format!(
                "model endpoint returned invalid JSON: {e}: {}",
                snippet(&text)
            ))
        })?;
        let completion = parse_completion(&value)?;
        stream_text(sink, &completion.content, 1, 64);
        Ok(completion)
    }
}

impl Driver for OpenAiDriver {
    fn name(&self) -> &str {
        "openai"
    }

    fn run<'a>(&'a self, turn: &'a Turn) -> BoxFuture<'a, Result<Reply>> {
        Box::pin(async move {
            let model = turn
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(&self.default_model)
                .to_string();
            let mut messages = build_messages(turn);
            let mut transcript = String::new();
            let mut usage = None;
            for iteration in 0..=MAX_TOOL_ITERATIONS {
                let allow_tools = iteration < MAX_TOOL_ITERATIONS;
                let segment = SegmentSink::new(turn.sink.as_ref(), !transcript.is_empty());
                let completion = self
                    .complete(&model, &messages, allow_tools, &segment)
                    .await?;
                if !completion.content.is_empty() {
                    if !transcript.is_empty() {
                        transcript.push_str(SEGMENT_SEPARATOR);
                    }
                    transcript.push_str(&completion.content);
                }
                if completion.usage.is_some() {
                    usage = completion.usage.clone();
                }
                messages.push(assistant_message(&completion));
                if completion.tool_calls.is_empty() || !allow_tools {
                    return Ok(Reply {
                        text: transcript,
                        usage,
                    });
                }
                for call in &completion.tool_calls {
                    let args: Value = serde_json::from_str(&call.arguments)
                        .unwrap_or_else(|_| json!({ "raw": call.arguments }));
                    turn.sink.tool_call(&call.name, &args);
                    tracing::info!(tool = %call.name, iteration, "running tool");
                    let output = run_tool(&turn.home, &turn.owner, &call.name, &args).await;
                    messages.push(
                        json!({ "role": "tool", "tool_call_id": call.id, "content": output }),
                    );
                }
            }
            Ok(Reply {
                text: transcript,
                usage,
            })
        })
    }
}

/// Inserted between the text of consecutive model calls within one turn.
const SEGMENT_SEPARATOR: &str = "\n\n";

/// Forwards deltas to the real sink, emitting a separator before the first
/// one when earlier text exists in this turn.
struct SegmentSink<'a> {
    inner: &'a dyn EventSink,
    separator_pending: std::sync::atomic::AtomicBool,
}

impl<'a> SegmentSink<'a> {
    fn new(inner: &'a dyn EventSink, needs_separator: bool) -> Self {
        SegmentSink {
            inner,
            separator_pending: std::sync::atomic::AtomicBool::new(needs_separator),
        }
    }
}

impl EventSink for SegmentSink<'_> {
    fn delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self
            .separator_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.inner.delta(SEGMENT_SEPARATOR);
        }
        self.inner.delta(text);
    }
    fn tool_call(&self, name: &str, arguments: &Value) {
        self.inner.tool_call(name, arguments);
    }
}

fn snippet(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= ERROR_BODY_CHARS {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(ERROR_BODY_CHARS).collect::<String>())
    }
}

/// The function tools offered to the model.
pub fn tools_spec() -> Value {
    json!([
        { "type": "function", "function": {
            "name": "run_shell",
            "description": "Run a shell command with /bin/sh in the home directory and return its combined output and exit code.",
            "parameters": { "type": "object", "properties": {
                "command": { "type": "string", "description": "The command line to run." }
            }, "required": ["command"] } } },
        { "type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file. Paths are relative to the home directory.",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string" }
            }, "required": ["path"] } } },
        { "type": "function", "function": {
            "name": "write_file",
            "description": "Write a text file, creating parent directories. Paths are relative to the home directory.",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            }, "required": ["path", "content"] } } },
        { "type": "function", "function": {
            "name": "list_dir",
            "description": "List a directory. Paths are relative to the home directory; '.' is the home.",
            "parameters": { "type": "object", "properties": {
                "path": { "type": "string" }
            }, "required": ["path"] } } }
    ])
}

fn system_prompt(home: &Path) -> String {
    format!(
        "You are an AI agent running inside your own persistent Linux computer. \
         Your home directory is {} and it is the only place that persists; use relative paths. \
         Use the tools to run shell commands and to read, write and list files. \
         Work step by step, check results, and finish with a clear answer for the user.",
        home.display()
    )
}

/// System prompt, session history and the current input as chat messages.
pub fn build_messages(turn: &Turn) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(&turn.home) })];
    for m in &turn.history {
        if m.role == "user" || m.role == "assistant" {
            messages.push(json!({ "role": m.role, "content": m.content }));
        }
    }
    let mut input = turn.input.clone();
    if !turn.files.is_empty() {
        input.push_str("\n\nAttached files (relative to the home directory): ");
        input.push_str(&turn.files.join(", "));
    }
    messages.push(json!({ "role": "user", "content": input }));
    messages
}

fn assistant_message(c: &Completion) -> Value {
    let mut m = json!({ "role": "assistant", "content": if c.content.is_empty() { Value::Null } else { Value::String(c.content.clone()) } });
    if !c.tool_calls.is_empty() {
        m["tool_calls"] = Value::Array(
            c.tool_calls
                .iter()
                .map(|t| json!({ "id": t.id, "type": "function", "function": { "name": t.name, "arguments": t.arguments } }))
                .collect(),
        );
    }
    m
}

/// Parse a non-streaming `chat.completion` object.
pub fn parse_completion(v: &Value) -> Result<Completion> {
    if let Some(err) = v.get("error") {
        return Err(Error::bad_gateway(format!("model endpoint error: {err}")));
    }
    let mut acc = Accumulator::default();
    acc.apply(v, None)?;
    Ok(acc.finish())
}

/// Merges streaming chunks (or one whole completion) into a [`Completion`].
#[derive(Default)]
struct Accumulator {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<Value>,
}

impl Accumulator {
    fn apply(&mut self, v: &Value, sink: Option<&dyn EventSink>) -> Result<()> {
        if let Some(err) = v.get("error") {
            return Err(Error::bad_gateway(format!("model endpoint error: {err}")));
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        for choice in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) else {
                continue;
            };
            if let Some(text) = content_text(delta.get("content"))
                && !text.is_empty()
            {
                if let Some(s) = sink {
                    s.delta(&text);
                }
                self.content.push_str(&text);
            }
            for (pos, tc) in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let index = tc
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|i| i as usize)
                    .unwrap_or(self.tool_calls.len() + pos);
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(ToolCall::default());
                }
                let slot = &mut self.tool_calls[index];
                if let Some(id) = tc.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    slot.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(Value::as_str)
                        && !name.is_empty()
                        && slot.name.is_empty()
                    {
                        slot.name = name.to_string();
                    }
                    match f.get("arguments") {
                        Some(Value::String(s)) => slot.arguments.push_str(s),
                        Some(Value::Null) | None => {}
                        Some(other) => slot.arguments.push_str(&other.to_string()),
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Completion {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|t| !t.name.is_empty())
            .enumerate()
            .map(|(i, mut t)| {
                if t.id.is_empty() {
                    t.id = format!("call_{i}");
                }
                if t.arguments.trim().is_empty() {
                    t.arguments = "{}".to_string();
                }
                t
            })
            .collect();
        Completion {
            content: self.content,
            tool_calls,
            usage: self.usage,
        }
    }
}

/// Message content as text: a string or an array of `{"type":"text","text":...}` parts.
fn content_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str).or_else(|| p.as_str()))
                .collect::<Vec<_>>()
                .concat(),
        ),
        _ => None,
    }
}

/// Position of the first blank line separating SSE events: `(block_end, next_start)`.
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|p| (p, p + 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, p + 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// The joined `data:` payload of one SSE event block, if any.
pub fn sse_data(block: &str) -> Option<String> {
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

async fn read_sse_stream(resp: reqwest::Response, sink: &dyn EventSink) -> Result<Completion> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut acc = Accumulator::default();
    let mut done = false;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| Error::bad_gateway(format!("error reading model stream: {e}")))?;
        buf.extend_from_slice(&chunk);
        while let Some((end, next)) = find_event_end(&buf) {
            let block = String::from_utf8_lossy(&buf[..end]).into_owned();
            buf.drain(..next);
            done |= apply_sse_block(&block, &mut acc, sink)?;
        }
        if done {
            break;
        }
    }
    if !done && !buf.is_empty() {
        let block = String::from_utf8_lossy(&buf).into_owned();
        apply_sse_block(&block, &mut acc, sink)?;
    }
    Ok(acc.finish())
}

/// Returns true on the `[DONE]` sentinel.
fn apply_sse_block(block: &str, acc: &mut Accumulator, sink: &dyn EventSink) -> Result<bool> {
    let Some(data) = sse_data(block) else {
        return Ok(false);
    };
    if data.trim() == "[DONE]" {
        return Ok(true);
    }
    let v: Value = serde_json::from_str(&data).map_err(|e| {
        Error::bad_gateway(format!("invalid stream chunk from model endpoint: {e}"))
    })?;
    acc.apply(&v, Some(sink))?;
    Ok(false)
}

/// Execute one tool locally: shell commands run as the home owner and files
/// written end up owned by it. Errors become text so the model can recover.
pub async fn run_tool(home: &Path, owner: &HomeOwner, name: &str, args: &Value) -> String {
    match run_tool_inner(home, owner, name, args).await {
        Ok(text) => text,
        Err(e) => format!("error: {}", describe_error(&e)),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid(format!("missing string argument `{key}`")))
}

async fn run_tool_inner(
    home: &Path,
    owner: &HomeOwner,
    name: &str,
    args: &Value,
) -> Result<String> {
    match name {
        "run_shell" => {
            let command = arg(args, "command")?;
            let out = run_shell_combined(home, owner, command, TOOL_SHELL_TIMEOUT, TOOL_OUTPUT_CAP)
                .await?;
            let mut text = out.stdout;
            if out.truncated {
                text.push_str("\n[output truncated]");
            }
            if out.exit_code != 0 {
                text.push_str(&format!("\n[exit code {}]", out.exit_code));
            }
            if text.is_empty() {
                text.push_str("(no output)");
            }
            Ok(text)
        }
        "read_file" => {
            let files = Home::open_as(home, owner.clone())?;
            let (bytes, truncated) = files.read_file(arg(args, "path")?, READ_FILE_CAP).await?;
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if truncated {
                text.push_str("\n[truncated]");
            }
            Ok(text)
        }
        "write_file" => {
            let files = Home::open_as(home, owner.clone())?;
            let (path, size) = files
                .write_file(arg(args, "path")?, arg(args, "content")?.as_bytes())
                .await?;
            Ok(format!("wrote {size} bytes to {path}"))
        }
        "list_dir" => {
            let files = Home::open_as(home, owner.clone())?;
            let listing = files
                .list(args.get("path").and_then(Value::as_str).unwrap_or("."))
                .await?;
            if listing.entries.is_empty() {
                return Ok("(empty directory)".to_string());
            }
            Ok(listing
                .entries
                .iter()
                .map(|e| {
                    if e.kind == "dir" {
                        format!("{}/", e.name)
                    } else {
                        format!("{} ({} bytes)", e.name, e.size)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        other => Err(Error::invalid(format!("unknown tool `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post};

    use super::*;
    use crate::drivers::{RecordingSink, test_turn};

    fn home() -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("warren-gw-openai-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_plain_completion() {
        let v = json!({ "choices": [{ "message": { "role": "assistant", "content": "hi", "tool_calls": [
            { "id": "c1", "type": "function", "function": { "name": "run_shell", "arguments": "{\"command\":\"ls\"}" } }
        ] } }], "usage": { "total_tokens": 3 } });
        let c = parse_completion(&v).unwrap();
        assert_eq!(c.content, "hi");
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "run_shell");
        assert_eq!(c.tool_calls[0].id, "c1");
        assert_eq!(c.usage.unwrap()["total_tokens"], 3);
        let parts = json!({ "choices": [{ "message": { "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] } }] });
        assert_eq!(parse_completion(&parts).unwrap().content, "ab");
        assert!(parse_completion(&json!({ "error": { "message": "nope" } })).is_err());
    }

    #[test]
    fn accumulates_stream_chunks() {
        let sink = RecordingSink::default();
        let mut acc = Accumulator::default();
        let chunks = [
            json!({ "choices": [{ "delta": { "role": "assistant", "content": "Hel" } }] }),
            json!({ "choices": [{ "delta": { "content": "lo" } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "call_a", "function": { "name": "read_file", "arguments": "{\"pa" } }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "th\":\"x\"}" } }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 1, "function": { "name": "list_dir", "arguments": "" } }] } }] }),
            json!({ "choices": [], "usage": { "total_tokens": 9 } }),
        ];
        for c in &chunks {
            acc.apply(c, Some(&sink)).unwrap();
        }
        let c = acc.finish();
        assert_eq!(c.content, "Hello");
        assert_eq!(sink.text(), "Hello");
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].arguments, "{\"path\":\"x\"}");
        assert_eq!(c.tool_calls[1].id, "call_1");
        assert_eq!(c.tool_calls[1].arguments, "{}");
        assert_eq!(c.usage.unwrap()["total_tokens"], 9);
    }

    #[test]
    fn sse_helpers() {
        assert_eq!(
            sse_data("event: x\ndata: {\"a\":1}\n"),
            Some("{\"a\":1}".to_string())
        );
        assert_eq!(sse_data(": comment\n"), None);
        assert_eq!(sse_data("data:[DONE]"), Some("[DONE]".to_string()));
        assert_eq!(find_event_end(b"data: a\n\ndata: b"), Some((7, 9)));
        assert_eq!(find_event_end(b"data: a\r\n\r\n"), Some((7, 11)));
        assert_eq!(find_event_end(b"data: a"), None);
        assert_eq!(snippet("short"), "short");
        assert!(snippet(&"x".repeat(2000)).ends_with('…'));
    }

    #[tokio::test]
    async fn tools_are_confined_and_return_text_errors() {
        let h = home();
        let o = HomeOwner::current();
        assert_eq!(
            run_tool(
                &h,
                &o,
                "write_file",
                &json!({ "path": "notes/a.txt", "content": "hey" })
            )
            .await,
            "wrote 3 bytes to notes/a.txt"
        );
        assert_eq!(
            run_tool(&h, &o, "read_file", &json!({ "path": "notes/a.txt" })).await,
            "hey"
        );
        assert_eq!(
            run_tool(&h, &o, "list_dir", &json!({ "path": "." })).await,
            "notes/"
        );
        assert_eq!(
            run_tool(&h, &o, "list_dir", &json!({ "path": "notes" })).await,
            "a.txt (3 bytes)"
        );
        let shell = run_tool(
            &h,
            &o,
            "run_shell",
            &json!({ "command": "cat notes/a.txt; exit 4" }),
        )
        .await;
        assert_eq!(shell, "hey\n[exit code 4]");
        assert_eq!(
            run_tool(&h, &o, "run_shell", &json!({ "command": "true" })).await,
            "(no output)"
        );
        assert!(
            run_tool(&h, &o, "read_file", &json!({ "path": "../../etc/passwd" }))
                .await
                .starts_with("error: ")
        );
        assert!(
            run_tool(&h, &o, "read_file", &json!({ "path": "missing" }))
                .await
                .starts_with("error: ")
        );
        assert!(
            run_tool(&h, &o, "read_file", &json!({}))
                .await
                .starts_with("error: missing string argument")
        );
        assert!(
            run_tool(&h, &o, "nope", &json!({}))
                .await
                .starts_with("error: unknown tool")
        );
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[test]
    fn message_building() {
        let sink = Arc::new(RecordingSink::default());
        let mut turn = test_turn("do it", std::path::PathBuf::from("/home/agent"), sink);
        turn.history = vec![
            warren_core::agentapi::Message {
                role: "user".into(),
                content: "earlier".into(),
                at: 1,
            },
            warren_core::agentapi::Message {
                role: "assistant".into(),
                content: "sure".into(),
                at: 2,
            },
            warren_core::agentapi::Message {
                role: "tool".into(),
                content: "skip".into(),
                at: 3,
            },
        ];
        turn.files = vec!["a.txt".into()];
        let m = build_messages(&turn);
        assert_eq!(m.len(), 4);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["content"], "earlier");
        assert_eq!(m[2]["role"], "assistant");
        assert!(m[3]["content"].as_str().unwrap().contains("Attached files"));
        let c = Completion {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "x".into(),
                arguments: "{}".into(),
            }],
            usage: None,
        };
        let am = assistant_message(&c);
        assert!(am["content"].is_null());
        assert_eq!(am["tool_calls"][0]["function"]["name"], "x");
        let d = OpenAiDriver::new("http://h/v1/", "k", "").unwrap();
        assert_eq!(d.endpoint(), "http://h/v1/chat/completions");
        assert_eq!(d.default_model, DEFAULT_MODEL);
    }

    #[derive(Clone)]
    struct Fake {
        calls: Arc<AtomicUsize>,
        seen: Arc<parking_lot::Mutex<Vec<Value>>>,
    }

    async fn fake_chat(
        State(f): State<Fake>,
        headers: HeaderMap,
        body: String,
    ) -> axum::response::Response {
        let v: Value = serde_json::from_str(&body).unwrap();
        f.seen.lock().push(v.clone());
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer test-key"
        );
        let n = f.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // First call: a plain JSON completion asking for a tool.
            let out = json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "write_file", "arguments": "{\"path\":\"out.txt\",\"content\":\"from tool\"}" } }
            ] } }] });
            return axum::Json(out).into_response();
        }
        // Second call: streamed text that references the tool result.
        let tool_msg = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .cloned()
            .unwrap();
        let text = format!("done: {}", tool_msg["content"].as_str().unwrap());
        let (a, b) = text.split_at(5);
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({ "choices": [{ "delta": { "content": a } }] }),
            json!({ "choices": [{ "delta": { "content": b } }] }),
            json!({ "choices": [], "usage": { "total_tokens": 12 } }),
        );
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response()
    }

    #[tokio::test]
    async fn agent_loop_against_fake_endpoint() {
        let fake = Fake {
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(fake_chat))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let h = home();
        let driver =
            OpenAiDriver::new(&format!("http://{addr}/v1"), "test-key", "test-model").unwrap();
        let sink = Arc::new(RecordingSink::default());
        let turn = test_turn("write a file", h.clone(), sink.clone());
        let reply = driver.run(&turn).await.unwrap();
        assert_eq!(reply.text, "done: wrote 9 bytes to out.txt");
        assert_eq!(sink.text(), reply.text);
        assert_eq!(reply.usage.unwrap()["total_tokens"], 12);
        assert_eq!(
            std::fs::read_to_string(h.join("out.txt")).unwrap(),
            "from tool"
        );
        let rec = sink.snapshot();
        assert_eq!(rec.tool_calls.len(), 1);
        assert_eq!(rec.tool_calls[0].0, "write_file");
        assert!(rec.deltas.len() >= 2);
        let seen = fake.seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["model"], "test-model");
        assert_eq!(seen[0]["stream"], true);
        assert_eq!(seen[1]["messages"].as_array().unwrap().len(), 4);
        assert_eq!(seen[1]["messages"][2]["tool_calls"][0]["id"], "call_1");
        std::fs::remove_dir_all(&h).unwrap();
    }

    #[tokio::test]
    async fn upstream_errors_are_reported() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "{\"error\":{\"message\":\"bad key\"}}",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let h = home();
        let driver = OpenAiDriver::new(&format!("http://{addr}/v1"), "", "m").unwrap();
        let sink = Arc::new(RecordingSink::default());
        let turn = test_turn("hi", h.clone(), sink);
        let e = driver.run(&turn).await.unwrap_err();
        assert_eq!(e.code, "bad_gateway");
        assert!(e.message.contains("401"), "{e}");
        assert!(e.message.contains("bad key"), "{e}");
        std::fs::remove_dir_all(&h).unwrap();
    }
}
