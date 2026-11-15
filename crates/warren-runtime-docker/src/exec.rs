//! `Runtime::exec` through Docker exec: `/bin/sh -lc <command>` with capped,
//! separately captured stdout and stderr and a wall-clock timeout.
//!
//! Without an explicit user the command runs as the account that owns the
//! instance's home on the host (see [`resolve_identity`]), so everything it
//! writes into the persistent disk stays removable by the daemon and the agent
//! does not work as root inside its own machine.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use bollard::{
    container::LogOutput,
    exec::{StartExecOptions, StartExecResults},
    models::ExecConfig,
};
use futures::StreamExt;
use warren_core::{
    DEFAULT_EXEC_TIMEOUT_SEC, Error, ExecRequest, ExecResult, Instance, MAX_EXEC_OUTPUT_BYTES,
    Result,
};

use crate::{DockerRuntime, container::env_pairs, error, runtime::target, storage};

/// Bytes of each stream attached to a timeout error.
const TAIL_BYTES: usize = 2048;
/// How long to wait for Docker to record the exit code once the output closed.
const EXIT_CODE_WAIT: Duration = Duration::from_secs(5);

/// Who an exec runs as and with which environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecIdentity {
    /// Docker `User`; `None` leaves the image default.
    pub(crate) user: Option<String>,
    /// Environment handed to the exec: the request's, plus `HOME` when defaulted.
    pub(crate) env: BTreeMap<String, String>,
}

/// Resolve the user and environment of an exec.
///
/// An explicit `req.user` (`root` included) is passed through unchanged with the
/// request's environment. An empty user runs as `home_owner`, the `uid:gid` that
/// owns the instance's home on the host at exec time: uid 1000 for the built-in
/// images, whatever created the directory otherwise, so files the command writes
/// into the persistent disk belong to the same account as the rest of it. Docker
/// gives a numeric uid without a passwd entry `HOME=/`, so `HOME` is set to
/// `mount_path` unless the request sets it. When the home directory is missing
/// (`home_owner` is `None`) the image default user applies.
pub(crate) fn resolve_identity(
    req: &ExecRequest,
    home_owner: Option<(u32, u32)>,
    mount_path: &str,
) -> ExecIdentity {
    if !req.user.is_empty() {
        return ExecIdentity {
            user: Some(req.user.clone()),
            env: req.env.clone(),
        };
    }
    let mut env = req.env.clone();
    if !mount_path.is_empty() {
        env.entry("HOME".to_string())
            .or_insert_with(|| mount_path.to_string());
    }
    ExecIdentity {
        user: home_owner.map(|(uid, gid)| format!("{uid}:{gid}")),
        env,
    }
}

/// One output stream capped at `cap` bytes. Bytes past the cap are dropped and
/// the buffer is marked truncated; the command itself keeps running.
#[derive(Debug)]
pub(crate) struct OutputBuffer {
    bytes: Vec<u8>,
    cap: usize,
    pub(crate) truncated: bool,
}

impl OutputBuffer {
    pub(crate) fn new(cap: usize) -> Self {
        OutputBuffer {
            bytes: Vec::new(),
            cap,
            truncated: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        let room = self.cap.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// The last `n` bytes as text.
    pub(crate) fn tail(&self, n: usize) -> String {
        let start = self.bytes.len().saturating_sub(n);
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }

    pub(crate) fn into_string(self) -> String {
        String::from_utf8(self.bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }
}

impl DockerRuntime {
    /// Run `req.command` inside the instance's container.
    pub(crate) async fn exec_impl(&self, inst: &Instance, req: &ExecRequest) -> Result<ExecResult> {
        if req.command.trim().is_empty() {
            return Err(Error::invalid("command must not be empty"));
        }
        let reference = target(inst);
        let timeout_seconds = if req.timeout_seconds == 0 {
            DEFAULT_EXEC_TIMEOUT_SEC
        } else {
            req.timeout_seconds
        };
        let started = Instant::now();
        let home_owner = if req.user.is_empty() {
            storage::owner(self.home_path(inst)).await
        } else {
            None
        };
        let identity = resolve_identity(req, home_owner, &inst.mount_path);
        if req.user.is_empty() {
            tracing::debug!(instance = %inst.id, user = identity.user.as_deref().unwrap_or("<image default>"), "exec runs as the home owner");
        }
        let config = ExecConfig {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(false),
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                req.command.clone(),
            ]),
            user: identity.user,
            working_dir: (!req.workdir.is_empty()).then(|| req.workdir.clone()),
            env: (!identity.env.is_empty()).then(|| env_pairs(&identity.env)),
            ..Default::default()
        };
        let created = match self.docker.create_exec(&reference, config).await {
            Ok(created) => created,
            Err(e) if error::is_not_found(&e) => return Err(Error::not_found("container")),
            Err(e) if error::is_conflict(&e) => {
                return Err(Error::conflict(format!(
                    "instance {} is not running",
                    inst.id
                )));
            }
            Err(e) => return Err(error::map(e, "container")),
        };
        let options = StartExecOptions {
            detach: false,
            tty: false,
            output_capacity: None,
        };
        let attached = self
            .docker
            .start_exec(&created.id, Some(options))
            .await
            .map_err(|e| error::map(e, "exec"))?;
        let StartExecResults::Attached { mut output, .. } = attached else {
            return Err(Error::internal("docker started the exec detached"));
        };
        let mut stdout = OutputBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let mut stderr = OutputBuffer::new(MAX_EXEC_OUTPUT_BYTES);
        let budget = Duration::from_secs(u64::from(timeout_seconds));
        let drained = tokio::time::timeout(budget, async {
            while let Some(item) = output.next().await {
                match item {
                    Ok(LogOutput::StdOut { message }) | Ok(LogOutput::Console { message }) => {
                        stdout.push(&message)
                    }
                    Ok(LogOutput::StdErr { message }) => stderr.push(&message),
                    Ok(LogOutput::StdIn { .. }) => {}
                    Err(e) => return Err(error::map(e, "exec")),
                }
            }
            Ok(())
        })
        .await;
        drop(output);
        match drained {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                tracing::warn!(instance = %inst.id, timeout_seconds, "exec timed out; the process may still run inside the container");
                return Err(Error::timeout(format!(
                    "command did not finish within {timeout_seconds} seconds"
                ))
                .with_details(serde_json::json!({
                    "timeout_seconds": timeout_seconds,
                    "stdout_tail": stdout.tail(TAIL_BYTES),
                    "stderr_tail": stderr.tail(TAIL_BYTES),
                })));
            }
        }
        let exit_code = self.exec_exit_code(&created.id).await?;
        let truncated = stdout.truncated || stderr.truncated;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        tracing::debug!(instance = %inst.id, exit_code, duration_ms, truncated, "exec finished");
        Ok(ExecResult {
            exit_code,
            stdout: stdout.into_string(),
            stderr: stderr.into_string(),
            duration_ms,
            truncated,
        })
    }

    /// The exit code of a finished exec. Docker records it a moment after the
    /// output closes, so this polls briefly; -1 means it never became known.
    async fn exec_exit_code(&self, exec_id: &str) -> Result<i32> {
        let deadline = Instant::now() + EXIT_CODE_WAIT;
        loop {
            let inspect = self
                .docker
                .inspect_exec(exec_id)
                .await
                .map_err(|e| error::map(e, "exec"))?;
            if inspect.running != Some(true) {
                return Ok(match inspect.exit_code {
                    Some(code) => i32::try_from(code).unwrap_or(i32::MAX),
                    None => {
                        tracing::warn!(exec = exec_id, "docker reported no exit code");
                        -1
                    }
                });
            }
            if Instant::now() >= deadline {
                tracing::warn!(exec = exec_id, "exec still running after its output closed");
                return Ok(-1);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffer_caps_and_marks_truncation() {
        let mut buf = OutputBuffer::new(5);
        buf.push(b"abc");
        assert!(!buf.truncated);
        buf.push(b"defg");
        assert!(buf.truncated);
        assert_eq!(buf.tail(2), "de");
        assert_eq!(buf.into_string(), "abcde");

        let mut full = OutputBuffer::new(0);
        full.push(b"x");
        assert!(full.truncated);
        assert_eq!(full.into_string(), "");

        let mut lossy = OutputBuffer::new(10);
        lossy.push(&[0xff, b'o', b'k']);
        assert_eq!(lossy.into_string(), "\u{fffd}ok");
    }

    fn request(user: &str, env: &[(&str, &str)]) -> ExecRequest {
        ExecRequest {
            command: "id".into(),
            user: user.into(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_to_the_home_owner_with_home_set() {
        let id = resolve_identity(
            &request("", &[("FOO", "bar")]),
            Some((1000, 100)),
            "/home/agent",
        );
        assert_eq!(id.user.as_deref(), Some("1000:100"));
        assert_eq!(id.env, env(&[("FOO", "bar"), ("HOME", "/home/agent")]));

        let id = resolve_identity(&request("", &[]), Some((0, 0)), "/workspace");
        assert_eq!(id.user.as_deref(), Some("0:0"));
        assert_eq!(id.env, env(&[("HOME", "/workspace")]));
    }

    #[test]
    fn explicit_users_pass_through_untouched() {
        for user in ["root", "1000:1000", "agent", "65534"] {
            let req = request(user, &[("FOO", "bar")]);
            let id = resolve_identity(&req, Some((1000, 1000)), "/home/agent");
            assert_eq!(id.user.as_deref(), Some(user));
            assert_eq!(id.env, req.env, "no HOME is added for {user}");
        }
    }

    #[test]
    fn missing_home_falls_back_to_the_image_default() {
        let id = resolve_identity(&request("", &[]), None, "/home/agent");
        assert_eq!(id.user, None);
        assert_eq!(id.env, env(&[("HOME", "/home/agent")]));
    }

    #[test]
    fn home_from_the_request_wins_and_needs_a_mount_path() {
        let id = resolve_identity(
            &request("", &[("HOME", "/tmp/elsewhere")]),
            Some((1000, 1000)),
            "/home/agent",
        );
        assert_eq!(id.user.as_deref(), Some("1000:1000"));
        assert_eq!(id.env, env(&[("HOME", "/tmp/elsewhere")]));

        let id = resolve_identity(&request("", &[]), Some((1000, 1000)), "");
        assert_eq!(id.user.as_deref(), Some("1000:1000"));
        assert!(id.env.is_empty(), "no mount path, no HOME");
    }
}
