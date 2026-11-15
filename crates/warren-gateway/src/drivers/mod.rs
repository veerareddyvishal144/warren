//! Agent drivers: the pluggable piece that turns one user input into one reply.
//!
//! A driver receives a [`Turn`] (the input, the session history, the home
//! directory and its owner, and an [`EventSink`] for streaming) and returns
//! the final text. Shell commands run as the home owner and files written by
//! tools belong to it, so a root gateway never leaves root-owned files behind.
//! Drivers are selected by `WARREN_AGENT_DRIVER`: `echo`, `openai` or
//! `command`.

pub mod command;
pub mod echo;
pub mod openai;

use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use warren_core::{Error, Result, agentapi::Message};

use crate::{config::GatewayConfig, owner::HomeOwner};

/// Receives streaming events while a turn runs.
pub trait EventSink: Send + Sync {
    /// A piece of the assistant's reply text.
    fn delta(&self, text: &str);
    /// A tool the driver is about to run.
    fn tool_call(&self, name: &str, arguments: &Value);
}

/// Everything a driver needs for one turn.
#[derive(Clone)]
pub struct Turn {
    pub response_id: String,
    pub session_id: String,
    pub input: String,
    /// Model requested by the client, if any.
    pub model: Option<String>,
    /// File paths the client attached (relative to the home).
    pub files: Vec<String>,
    /// Earlier messages of the session, oldest first.
    pub history: Vec<Message>,
    /// The persistent home directory.
    pub home: PathBuf,
    /// Who shell commands run as and who created files belong to.
    pub owner: HomeOwner,
    pub sink: Arc<dyn EventSink>,
}

/// A finished turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// Provider usage object when available.
    pub usage: Option<Value>,
}

impl Reply {
    pub fn text(text: impl Into<String>) -> Self {
        Reply {
            text: text.into(),
            usage: None,
        }
    }
}

/// One agent backend.
pub trait Driver: Send + Sync {
    /// Name reported by `/v1/health`.
    fn name(&self) -> &str;
    /// Run one turn, streaming deltas to `turn.sink`, and return the full reply.
    fn run<'a>(&'a self, turn: &'a Turn) -> BoxFuture<'a, Result<Reply>>;
}

/// Build the configured driver. `Ok(None)` means the Agent API is disabled
/// (non-agent template or misconfiguration, which is logged).
pub fn build(config: &GatewayConfig) -> Result<Option<Arc<dyn Driver>>> {
    if !config.agent_enabled() {
        return Ok(None);
    }
    match config.driver.as_str() {
        "echo" => Ok(Some(Arc::new(echo::EchoDriver))),
        "openai" => Ok(Some(Arc::new(openai::OpenAiDriver::new(
            &config.openai_base_url,
            &config.openai_api_key,
            &config.model,
        )?))),
        "command" => Ok(Some(Arc::new(command::CommandDriver::new(
            &config.agent_command,
        )?))),
        other => Err(Error::invalid(format!(
            "unknown agent driver {other:?}; expected echo, openai, command or none"
        ))),
    }
}

/// Split `text` into at least `min_parts` pieces of at most `max_chars`
/// characters each, on char boundaries. Short texts are padded with empty
/// pieces so streaming clients always see `min_parts` deltas.
pub fn chunk_text(text: &str, min_parts: usize, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let max_chars = max_chars.max(1);
    let mut size = max_chars;
    if min_parts > 0 && chars.len().div_ceil(size) < min_parts {
        size = chars.len().div_ceil(min_parts).max(1);
    }
    let mut out: Vec<String> = chars.chunks(size).map(|c| c.iter().collect()).collect();
    while out.len() < min_parts {
        out.push(String::new());
    }
    out
}

/// Emit `text` through `sink` in chunks; see [`chunk_text`].
pub fn stream_text(sink: &dyn EventSink, text: &str, min_parts: usize, max_chars: usize) {
    for piece in chunk_text(text, min_parts, max_chars) {
        sink.delta(&piece);
    }
}

/// Human-readable reason a turn failed, for the `error` field of a response.
pub fn describe_error(e: &Error) -> String {
    if e.is_internal()
        && let Some(cause) = e
            .details
            .as_ref()
            .and_then(|d| d.get("cause"))
            .and_then(|c| c.as_str())
    {
        return cause.to_string();
    }
    e.message.clone()
}

/// An [`EventSink`] that records everything; handy for tests.
#[derive(Default)]
pub struct RecordingSink {
    inner: Mutex<Recorded>,
}

/// What a [`RecordingSink`] saw.
#[derive(Default, Clone, Debug)]
pub struct Recorded {
    pub deltas: Vec<String>,
    pub tool_calls: Vec<(String, Value)>,
}

impl RecordingSink {
    pub fn snapshot(&self) -> Recorded {
        self.inner.lock().clone()
    }
    /// All deltas concatenated.
    pub fn text(&self) -> String {
        self.inner.lock().deltas.concat()
    }
}

impl EventSink for RecordingSink {
    fn delta(&self, text: &str) {
        self.inner.lock().deltas.push(text.to_string());
    }
    fn tool_call(&self, name: &str, arguments: &Value) {
        self.inner
            .lock()
            .tool_calls
            .push((name.to_string(), arguments.clone()));
    }
}

/// A [`Turn`] for tests and tools with sensible defaults.
pub fn test_turn(input: &str, home: PathBuf, sink: Arc<dyn EventSink>) -> Turn {
    Turn {
        response_id: "resp".into(),
        session_id: "sess".into(),
        input: input.into(),
        model: None,
        files: Vec::new(),
        history: Vec::new(),
        home,
        owner: HomeOwner::current(),
        sink,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking() {
        assert_eq!(chunk_text("", 2, 64), vec!["", ""]);
        assert_eq!(chunk_text("a", 2, 64), vec!["a", ""]);
        assert_eq!(chunk_text("abcd", 2, 64), vec!["ab", "cd"]);
        assert_eq!(chunk_text("abcde", 2, 64), vec!["abc", "de"]);
        assert_eq!(chunk_text("héllo wörld", 1, 4), vec!["héll", "o wö", "rld"]);
        let parts = chunk_text(&"x".repeat(1000), 2, 64);
        assert_eq!(parts.len(), 16);
        assert_eq!(parts.concat().len(), 1000);
    }

    #[test]
    fn building_drivers() {
        let mut c = GatewayConfig::default();
        assert!(build(&c).unwrap().is_none());
        c.driver = "none".into();
        assert!(build(&c).unwrap().is_none());
        c.driver = "echo".into();
        assert_eq!(build(&c).unwrap().unwrap().name(), "echo");
        c.driver = "command".into();
        assert!(build(&c).is_err());
        c.agent_command = "cat".into();
        assert_eq!(build(&c).unwrap().unwrap().name(), "command");
        c.driver = "openai".into();
        assert_eq!(build(&c).unwrap().unwrap().name(), "openai");
        c.driver = "bogus".into();
        assert!(build(&c).is_err());
        let internal = Error::internal("boom");
        assert_eq!(describe_error(&internal), "boom");
        assert_eq!(describe_error(&Error::invalid("bad")), "bad");
    }
}
