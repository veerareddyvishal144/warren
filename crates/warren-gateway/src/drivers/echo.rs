//! The deterministic `echo` driver used by tests: replies `echo: <input>`;
//! an input starting with `!` runs the rest as a shell command (as the home
//! owner, in the home) and replies with its combined output. The reply is
//! always streamed in at least two deltas.

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::json;
use warren_core::{Error, Result, types::MAX_EXEC_OUTPUT_BYTES};

use super::{Driver, Reply, Turn, stream_text};
use crate::exec::run_shell_combined;

/// Shell commands run through the echo driver are killed after this long.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(600);
/// Deltas carry at most this many characters.
const DELTA_CHARS: usize = 64;

/// See the module documentation.
pub struct EchoDriver;

impl Driver for EchoDriver {
    fn name(&self) -> &str {
        "echo"
    }

    fn run<'a>(&'a self, turn: &'a Turn) -> BoxFuture<'a, Result<Reply>> {
        Box::pin(async move {
            let reply = match turn.input.strip_prefix('!') {
                Some(rest) => {
                    let command = rest.trim_start_matches('!').trim();
                    if command.is_empty() {
                        return Err(Error::invalid("no shell command after '!'"));
                    }
                    turn.sink
                        .tool_call("run_shell", &json!({ "command": command }));
                    let out = run_shell_combined(
                        &turn.home,
                        &turn.owner,
                        command,
                        SHELL_TIMEOUT,
                        MAX_EXEC_OUTPUT_BYTES,
                    )
                    .await?;
                    let mut text = out.stdout;
                    if out.truncated {
                        text.push_str("\n[output truncated]");
                    }
                    if text.is_empty() && out.exit_code != 0 {
                        text = format!("[exit code {}]", out.exit_code);
                    }
                    text
                }
                None => format!("echo: {}", turn.input),
            };
            stream_text(turn.sink.as_ref(), &reply, 2, DELTA_CHARS);
            Ok(Reply::text(reply))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::drivers::{RecordingSink, test_turn};

    fn home() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("warren-gw-echo-{}", warren_core::ids::new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn echoes_in_two_or_more_deltas() {
        let sink = Arc::new(RecordingSink::default());
        let turn = test_turn("hello", home(), sink.clone());
        let reply = EchoDriver.run(&turn).await.unwrap();
        assert_eq!(reply.text, "echo: hello");
        let rec = sink.snapshot();
        assert!(rec.deltas.len() >= 2, "{rec:?}");
        assert_eq!(sink.text(), "echo: hello");
        assert!(rec.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn bang_runs_shell_with_combined_output() {
        let sink = Arc::new(RecordingSink::default());
        let h = home();
        let turn = test_turn("!echo out; echo err 1>&2", h.clone(), sink.clone());
        let reply = EchoDriver.run(&turn).await.unwrap();
        assert_eq!(reply.text, "out\nerr\n");
        assert_eq!(sink.text(), "out\nerr\n");
        assert_eq!(sink.snapshot().tool_calls[0].0, "run_shell");

        let sink2 = Arc::new(RecordingSink::default());
        let turn = test_turn("!!exit 7", h.clone(), sink2.clone());
        assert_eq!(EchoDriver.run(&turn).await.unwrap().text, "[exit code 7]");
        let turn = test_turn("!   ", h.clone(), sink2);
        assert_eq!(EchoDriver.run(&turn).await.unwrap_err().status, 400);
        std::fs::remove_dir_all(&h).unwrap();
    }
}
