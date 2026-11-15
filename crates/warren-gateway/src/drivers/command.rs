//! The `command` driver runs `WARREN_AGENT_COMMAND` once per turn with the
//! prompt substituted for `{prompt}` (or appended as the last argument), in
//! the home directory and as the home owner. Its stdout is the reply and is
//! streamed as it arrives.

use std::{process::Stdio, time::Duration};

use futures::future::BoxFuture;
use tokio::io::AsyncReadExt;
use warren_core::{Error, Result, types::MAX_EXEC_OUTPUT_BYTES};

use super::{Driver, Reply, Turn};
use crate::exec::{exit_code, kill_tree, owner_command, read_capped};

/// A turn is killed after this long.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Placeholder replaced by the prompt.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";
/// Stderr kept for diagnostics.
const STDERR_CAP: usize = 64 * 1024;

/// See the module documentation.
pub struct CommandDriver {
    argv: Vec<String>,
}

impl CommandDriver {
    /// Parse the command template with shell-like quoting.
    pub fn new(template: &str) -> Result<Self> {
        let argv = split_words(template)?;
        if argv.is_empty() {
            return Err(Error::invalid("WARREN_AGENT_COMMAND is empty"));
        }
        Ok(CommandDriver { argv })
    }

    /// The argument vector for one prompt.
    pub fn argv_for(&self, prompt: &str) -> Vec<String> {
        let mut substituted = false;
        let mut out: Vec<String> = self
            .argv
            .iter()
            .map(|w| {
                if w.contains(PROMPT_PLACEHOLDER) {
                    substituted = true;
                    w.replace(PROMPT_PLACEHOLDER, prompt)
                } else {
                    w.clone()
                }
            })
            .collect();
        if !substituted {
            out.push(prompt.to_string());
        }
        out
    }
}

impl Driver for CommandDriver {
    fn name(&self) -> &str {
        "command"
    }

    fn run<'a>(&'a self, turn: &'a Turn) -> BoxFuture<'a, Result<Reply>> {
        Box::pin(async move {
            let argv = self.argv_for(&turn.input);
            let mut cmd = owner_command(&turn.owner, &argv)?;
            cmd.current_dir(&turn.home)
                .env("HOME", &turn.home)
                .env("WARREN_SESSION_ID", &turn.session_id)
                .env("WARREN_RESPONSE_ID", &turn.response_id)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .process_group(0);
            if let Some(model) = &turn.model {
                cmd.env("WARREN_REQUEST_MODEL", model);
            }
            let mut child = cmd.spawn().map_err(|e| {
                Error::internal(format!("cannot start agent command {:?}: {e}", argv[0]))
            })?;
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let sink = turn.sink.clone();
            let body = async {
                let (reply, err) = tokio::join!(
                    async {
                        let Some(mut out) = stdout else {
                            return (String::new(), false);
                        };
                        let mut text = String::new();
                        let mut pending: Vec<u8> = Vec::new();
                        let mut chunk = [0u8; 8192];
                        let mut truncated = false;
                        loop {
                            match out.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if text.len() + pending.len() >= MAX_EXEC_OUTPUT_BYTES {
                                        truncated = true;
                                        continue;
                                    }
                                    pending.extend_from_slice(&chunk[..n]);
                                    let valid = utf8_prefix_len(&pending);
                                    if valid > 0 {
                                        let piece =
                                            String::from_utf8_lossy(&pending[..valid]).into_owned();
                                        sink.delta(&piece);
                                        text.push_str(&piece);
                                        pending.drain(..valid);
                                    }
                                }
                            }
                        }
                        if !pending.is_empty() {
                            let piece = String::from_utf8_lossy(&pending).into_owned();
                            sink.delta(&piece);
                            text.push_str(&piece);
                        }
                        (text, truncated)
                    },
                    read_capped(stderr, STDERR_CAP)
                );
                let status = child.wait().await;
                (reply, err, status)
            };
            match tokio::time::timeout(COMMAND_TIMEOUT, body).await {
                Ok(((mut text, truncated), (stderr, _), status)) => {
                    let status = status?;
                    let code = exit_code(status);
                    let stderr = String::from_utf8_lossy(&stderr);
                    if code != 0 {
                        tracing::warn!(code, stderr = %stderr.trim(), "agent command exited with an error");
                        if text.trim().is_empty() {
                            let tail: String = stderr
                                .chars()
                                .rev()
                                .take(2000)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            return Err(Error::internal(format!(
                                "agent command exited with code {code}: {}",
                                tail.trim()
                            )));
                        }
                    }
                    if truncated {
                        let note = "\n[output truncated]";
                        turn.sink.delta(note);
                        text.push_str(note);
                    }
                    Ok(Reply::text(text))
                }
                Err(_) => {
                    kill_tree(&mut child).await;
                    Err(Error::timeout(format!(
                        "agent command did not finish within {} seconds",
                        COMMAND_TIMEOUT.as_secs()
                    )))
                }
            }
        })
    }
}

/// Length of the longest valid UTF-8 prefix (an incomplete trailing sequence is left out).
fn utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => {
            if e.error_len().is_some() {
                // Invalid byte: take everything, the lossy conversion handles it.
                bytes.len()
            } else {
                e.valid_up_to()
            }
        }
    }
}

/// Split a command line into words with POSIX-like quoting: single quotes are
/// literal, double quotes allow backslash escapes, backslash escapes outside quotes.
pub fn split_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(Error::invalid("unterminated single quote in command")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                            Some(other) => {
                                cur.push('\\');
                                cur.push(other);
                            }
                            None => {
                                return Err(Error::invalid("unterminated double quote in command"));
                            }
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(Error::invalid("unterminated double quote in command")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(ch) => cur.push(ch),
                    None => cur.push('\\'),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::drivers::{RecordingSink, test_turn};

    fn home() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("warren-gw-cmd-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn word_splitting_and_substitution() {
        assert_eq!(
            split_words("a  b\t'c d' \"e \\\"f\\\"\" g\\ h").unwrap(),
            vec!["a", "b", "c d", "e \"f\"", "g h"]
        );
        assert_eq!(split_words("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_words("x ''").unwrap(), vec!["x", ""]);
        assert!(split_words("'open").is_err());
        assert!(split_words("\"open").is_err());
        let d = CommandDriver::new("agent --prompt={prompt} -q").unwrap();
        assert_eq!(
            d.argv_for("hi there"),
            vec!["agent", "--prompt=hi there", "-q"]
        );
        let d = CommandDriver::new("agent run").unwrap();
        assert_eq!(d.argv_for("p"), vec!["agent", "run", "p"]);
        assert!(CommandDriver::new("").is_err());
        assert_eq!(utf8_prefix_len("héllo".as_bytes()), 6);
        assert_eq!(utf8_prefix_len(&"héllo".as_bytes()[..2]), 1);
    }

    #[tokio::test]
    async fn runs_command_and_streams_stdout() {
        let h = home();
        let d =
            CommandDriver::new("/bin/sh -c 'printf \"reply to: %s\" \"$1\"; echo warn 1>&2' sh")
                .unwrap();
        let sink = Arc::new(RecordingSink::default());
        let turn = test_turn("hello", h.clone(), sink.clone());
        let r = d.run(&turn).await.unwrap();
        assert_eq!(r.text, "reply to: hello");
        assert_eq!(sink.text(), "reply to: hello");
        assert!(!sink.snapshot().deltas.is_empty());

        let d = CommandDriver::new("/bin/sh -c 'echo broken 1>&2; exit 2'").unwrap();
        let e = d.run(&turn).await.unwrap_err();
        assert!(crate::drivers::describe_error(&e).contains("broken"), "{e}");

        let d = CommandDriver::new("/definitely/missing/binary").unwrap();
        assert!(d.run(&turn).await.is_err());
        std::fs::remove_dir_all(&h).unwrap();
    }
}
